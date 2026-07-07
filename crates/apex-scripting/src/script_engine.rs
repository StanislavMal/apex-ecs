//! `ScriptEngine` — the central control point for Lua scripting.
//!
//! # Lifecycle
//!
//! ```text
//! ScriptEngine::new()
//!   └── lua.globals().set(...)    ← register API functions
//!   └── register_component::<T>() ← register components
//!       └── load_scripts()        ← compiles all .lua files
//!
//! // Game loop:
//! loop {
//!     engine.poll_hot_reload();   ← checks for file changes
//!     engine.run(dt, &mut world); ← runs the active script
//!     world.tick();
//! }
//! ```

use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant},
};

use notify::{Event, EventKind, RecursiveMode, Watcher};

use apex_core::commands::Commands;
use apex_core::world::World;

use crate::{
    context::{ComponentBinding, ScriptContext},
    error::ScriptError,
    registrar::{EventBinding, ResourceBinding, ScriptableRegistrar},
    lua_api,
};

// ── CompiledScript ─────────────────────────────────────────────

struct CompiledScript {
    chunk_key: mlua::RegistryKey,
    /// Sandbox environment (_ENV) for script isolation
    env_key:   mlua::RegistryKey,
    #[allow(dead_code)]
    path: PathBuf,
}

// ── ScriptEngine ───────────────────────────────────────────────

pub struct ScriptEngine {
    /// The Lua VM. Held in an `Rc` so phase-B NonSend script-system runners can
    /// share it (they hold their own clone and call the registered function each
    /// frame); the VM stays single-threaded (`!Send`), which is exactly why those
    /// systems are NonSend (main-thread) — wave 3 §5.
    lua:            Rc<mlua::Lua>,
    ctx:            Rc<RefCell<ScriptContext>>,
    scripts:        HashMap<String, CompiledScript>,
    active_script:  String,
    script_dir:     Option<PathBuf>,
    watcher:        Option<Box<dyn Watcher>>,
    watch_rx:       Option<mpsc::Receiver<notify::Result<Event>>>,
    last_reload:    HashMap<String, Instant>,
    registered_components: Vec<String>,
    /// Scheduler ids of the script systems this engine registered (phase B). Held
    /// so a re-registration (hot-reload) can remove the previous set first, making
    /// [`register_systems`](Self::register_systems) idempotent.
    registered_system_ids: Vec<apex_scheduler::SystemId>,
}

impl ScriptEngine {
    // ── Constructor ────────────────────────────────────────────

    pub fn new() -> Self {
        let lua = Rc::new(mlua::Lua::new());

        let ctx = Rc::new(RefCell::new(ScriptContext::new()));

        if let Err(e) = lua_api::register_globals(&lua) {
            log::error!("ScriptEngine: error registering Lua API: {}", e);
        }

        lua.set_app_data(ctx.clone());

        Self {
            lua,
            ctx,
            scripts:        HashMap::new(),
            active_script:  String::new(),
            script_dir:     None,
            watcher:        None,
            watch_rx:       None,
            last_reload:    HashMap::new(),
            registered_components: Vec::new(),
            registered_system_ids: Vec::new(),
        }
    }

    pub fn with_dir(script_dir: &Path) -> Self {
        let mut this = Self::new();

        let (tx, rx) = mpsc::channel();

        let watcher_result = notify::recommended_watcher(
            move |res: notify::Result<Event>| {
                let _ = tx.send(res);
            }
        );

        match watcher_result {
            Ok(mut w) => {
                if let Err(e) = w.watch(script_dir, RecursiveMode::Recursive) {
                    log::warn!("ScriptEngine: failed to set watcher on {:?}: {}", script_dir, e);
                } else {
                    log::debug!("ScriptEngine: watching {:?}", script_dir);
                    this.watcher  = Some(Box::new(w));
                    this.watch_rx = Some(rx);
                }
            }
            Err(e) => {
                log::warn!("ScriptEngine: failed to create watcher: {}", e);
            }
        }

        this.script_dir = Some(script_dir.to_path_buf());
        this
    }

    // ── Sandbox ─────────────────────────────────────────────────

    /// Create an isolated environment for a script.
    /// Includes the Lua standard libraries, API functions, and component constructors.
    fn make_sandbox_env(&self) -> mlua::Result<mlua::Table> {
        let env = self.lua.create_table()?;

        // Standard libraries
        for name in &["math", "string", "table", "ipairs", "pairs", "next",
                      "select", "tonumber", "tostring", "type", "unpack"] {
            if let Ok(val) = self.lua.globals().get::<mlua::Value>(*name) {
                env.set(*name, val)?;
            }
        }

        // API functions
        for name in &["delta_time", "entity_count", "query", "commit", "system",
                      "spawn_entity", "despawn", "read_resource", "write_resource",
                      "emit_event", "log", "print", "log_debug", "log_warn", "log_error",
                      "inspect"] {
            if let Ok(val) = self.lua.globals().get::<mlua::Value>(*name) {
                env.set(*name, val)?;
            }
        }

        // Component constructors (Position.new, Velocity.new, ...)
        for name in &self.registered_components {
            if let Ok(val) = self.lua.globals().get::<mlua::Value>(name.as_str()) {
                env.set(name.as_str(), val)?;
            }
        }

        Ok(env)
    }

    // ── Component registration ─────────────────────────────────

    pub fn register_component<T>(&mut self, world: &World)
    where
        T: ScriptableRegistrar + apex_core::component::Component,
    {
        let comp_id = match world.registry().get_id::<T>() {
            Some(id) => id,
            None => {
                log::warn!(
                    "ScriptEngine::register_component: {} is not registered in World. \
                     Call world.register_component::<{}>() first.",
                    T::type_name_str(),
                    T::type_name_str(),
                );
                return;
            }
        };

        let binding = ComponentBinding {
            name: T::type_name_str(),
            id:   comp_id,
            type_id: std::any::TypeId::of::<T>(),
            read: |ptr: *const u8, lua: &mlua::Lua| -> mlua::Result<mlua::Value> {
                let val = unsafe { &*(ptr as *const T) };
                val.to_lua(lua)
            },
            write: |ptr: *mut u8, value: &mlua::Value| -> bool {
                if let Some(new_val) = T::from_lua(value) {
                    unsafe { *(ptr as *mut T) = new_val; }
                    true
                } else {
                    log::warn!("commit: failed to convert Lua value to {}", T::type_name_str());
                    false
                }
            },
        };

        self.ctx.borrow_mut().add_binding(binding);

        if let Err(e) = T::register_lua_type(&self.lua) {
            log::error!("ScriptEngine: error registering {}: {}", T::type_name_str(), e);
        }

        self.registered_components.push(T::type_name_str().to_string());

        // Spawn appliers insert the component via a `Commands` buffer (deterministic
        // ids + no `&mut World` from a script system). Registered on the shared
        // `ScriptContext` so both the monolith and a script-system runner reach them.
        let type_name_lower = T::type_name_str().to_lowercase();
        self.ctx.borrow_mut().add_spawn_applier(
            type_name_lower,
            Box::new(move |val: &mlua::Value, entity: apex_core::Entity, commands: &mut apex_core::commands::Commands| {
                if let Some(component) = T::from_lua(val) {
                    commands.insert(entity, component);
                } else {
                    log::warn!("spawn: failed to convert Lua value to {}", T::type_name_str());
                }
            }),
        );

        let exact_name = T::type_name_str().to_string();
        if exact_name.to_lowercase() != exact_name {
            self.ctx.borrow_mut().add_spawn_applier(
                exact_name,
                Box::new(move |val: &mlua::Value, entity: apex_core::Entity, commands: &mut apex_core::commands::Commands| {
                    if let Some(component) = T::from_lua(val) {
                        commands.insert(entity, component);
                    }
                }),
            );
        }

        log::debug!("ScriptEngine: registered component '{}'", T::type_name_str());
    }

    // ── Resource registration ──────────────────────────────────

    pub fn register_resource<T>(&mut self)
    where
        T: ScriptableRegistrar + Send + Sync,
    {
        let binding = ResourceBinding {
            name: T::type_name_str(),
            read: |lua: &mlua::Lua, world: &World| -> mlua::Result<mlua::Value> {
                let res = world.try_resource::<T>()
                    .ok_or_else(|| mlua::Error::runtime(format!("resource '{}' not found", T::type_name_str())))?;
                res.to_lua(lua)
            },
            write: |value: &mlua::Value, world: &mut World| -> bool {
                if let Some(new_val) = T::from_lua(value) {
                    world.insert_resource(new_val);
                    true
                } else {
                    log::warn!("write_resource: failed to convert Lua value to {}", T::type_name_str());
                    false
                }
            },
        };
        self.ctx.borrow_mut().add_resource_binding(binding);
        if let Err(e) = T::register_lua_type(&self.lua) {
            log::error!("ScriptEngine: error registering constructor {}: {}", T::type_name_str(), e);
        }
        self.registered_components.push(T::type_name_str().to_string());
        log::debug!("ScriptEngine: registered resource '{}'", T::type_name_str());
    }

    // ── Event registration ─────────────────────────────────────

    pub fn register_event<T>(&mut self)
    where
        T: ScriptableRegistrar + Send + Sync,
    {
        let binding = EventBinding {
            name: T::type_name_str(),
            emit: |value: &mlua::Value, world: &mut World| -> bool {
                if let Some(event) = T::from_lua(value) {
                    // send_event auto-registers the type — sending always succeeds.
                    world.send_event(event);
                    true
                } else {
                    log::warn!("emit_event: failed to convert Lua value to {}", T::type_name_str());
                    false
                }
            },
        };
        self.ctx.borrow_mut().add_event_binding(binding);
        if let Err(e) = T::register_lua_type(&self.lua) {
            log::error!("ScriptEngine: error registering constructor {}: {}", T::type_name_str(), e);
        }
        self.registered_components.push(T::type_name_str().to_string());
        log::debug!("ScriptEngine: registered event '{}'", T::type_name_str());
    }

    // ── Script loading ─────────────────────────────────────────

    pub fn load_scripts(&mut self) -> Result<(), ScriptError> {
        let script_dir = self.script_dir.clone().ok_or(ScriptError::NoScriptDir)?;

        let entries = std::fs::read_dir(&script_dir)
            .map_err(|e| ScriptError::io(script_dir.to_string_lossy(), e))?;

        let mut first_name: Option<String> = None;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lua") {
                continue;
            }

            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed")
                .to_string();

            match self.compile_file(&path) {
                Ok((chunk_key, env_key)) => {
                    log::info!("ScriptEngine: loaded script '{}'", name);
                    self.scripts.insert(name.clone(), CompiledScript { chunk_key, env_key, path });
                    if first_name.is_none() {
                        first_name = Some(name);
                    }
                }
                Err(e) => {
                    log::error!("ScriptEngine: compilation error '{}': {}", name, e);
                    return Err(e);
                }
            }
        }

        if let Some(name) = first_name {
            if self.active_script.is_empty() {
                self.active_script = name;
            }
        }

        Ok(())
    }

    pub fn load_script_str(&mut self, name: impl Into<String>, code: &str) -> Result<(), ScriptError> {
        let name = name.into();

        let env = self.make_sandbox_env()
            .map_err(|e| ScriptError::compile(&name, e))?;
        let env_key = self.lua.create_registry_value(mlua::Value::Table(env))
            .map_err(|e| ScriptError::runtime(&name, e))?;

        // Recreate env from the registry — set_environment takes ownership
        let env: mlua::Table = self.lua.registry_value(&env_key)
            .map_err(|e| ScriptError::runtime(&name, e))?;

        let chunk = self.lua.load(code)
            .set_name(&name)
            .set_environment(env)
            .into_function()
            .map_err(|e| ScriptError::compile(&name, e))?;

        let chunk_key = self.lua.create_registry_value(chunk)
            .map_err(|e| ScriptError::runtime(&name, e))?;

        self.scripts.insert(name.clone(), CompiledScript {
            chunk_key,
            env_key,
            path: PathBuf::from(format!("<{}>", name)),
        });

        if self.active_script.is_empty() {
            self.active_script = name;
        }

        Ok(())
    }

    pub fn set_active(&mut self, name: impl Into<String>) -> Result<(), ScriptError> {
        let name = name.into();
        if self.scripts.contains_key(&name) {
            self.active_script = name;
            Ok(())
        } else {
            Err(ScriptError::NotFound(name))
        }
    }

    // ── Execution ──────────────────────────────────────────────

    pub fn run(&mut self, dt: f32, world: &mut World) {
        if self.active_script.is_empty() {
            return;
        }

        {
            let mut ctx = self.ctx.borrow_mut();
            ctx.delta_time = dt;
            unsafe { ctx.set_world_ptr(world); }
        }

        if let Some(script) = self.scripts.get(&self.active_script) {
            let chunk: mlua::Function = match self.lua.registry_value(&script.chunk_key) {
                Ok(f) => f,
                Err(e) => {
                    log::error!("ScriptEngine: failed to get chunk '{}': {}", self.active_script, e);
                    self.ctx.borrow_mut().clear_world_ptr();
                    return;
                }
            };

            // Run the chunk — defines functions in the sandbox _ENV
            if let Err(e) = chunk.call::<()>(()) {
                log::error!("ScriptEngine: execution error '{}': {}", self.active_script, e);
            }

            // Call run() from the sandbox environment
            let env: mlua::Table = match self.lua.registry_value(&script.env_key) {
                Ok(t) => t,
                Err(e) => {
                    log::error!("ScriptEngine: failed to get sandbox '{}': {}", self.active_script, e);
                    self.ctx.borrow_mut().clear_world_ptr();
                    return;
                }
            };
            match env.get::<mlua::Function>("run") {
                Ok(run_fn) => {
                    if let Err(e) = run_fn.call::<()>(()) {
                        log::error!("ScriptEngine: error in run() '{}': {}", self.active_script, e);
                    }
                }
                Err(_) => {
                    // run() is not defined — script without a function, only top-level code (already run)
                }
            }
        } else {
            log::warn!("ScriptEngine::run: active script '{}' not found", self.active_script);
            self.ctx.borrow_mut().clear_world_ptr();
            return;
        }

        // Resource writes / events need `&mut World` (via the still-set world_ptr);
        // apply them first.
        {
            let mut ctx = self.ctx.borrow_mut();
            ctx.apply_deferred_resources_and_events(&self.lua);
        }

        // Despawns + spawns drain into a `Commands` buffer (deterministic ids via
        // the world's reserver), then applied — the same path a script SYSTEM uses
        // through its per-system slot.
        {
            let mut commands = Commands::new();
            commands.set_reserver(world.entity_reserver());
            self.ctx.borrow_mut().apply_structural_to_commands(&self.lua, &mut commands);
            commands.apply(world);
        }

        self.ctx.borrow_mut().clear_world_ptr();
    }

    // ── Phase-B: script systems as scheduler systems ──────────

    /// Set the frame delta time seen by phase-B script systems. Call once before
    /// `scheduler.run(world)` (all script systems share the one VM/context, so
    /// they observe the same `delta_time()` for the frame).
    pub fn set_delta_time(&mut self, dt: f32) {
        self.ctx.borrow_mut().delta_time = dt;
    }

    /// Register the active script's `system{}` declarations as first-class
    /// scheduler systems (phase B).
    ///
    /// Runs the active script's chunk to collect its `system{ name, query, fn }`
    /// declarations, translates each `query` into a scheduler
    /// [`AccessDescriptor`](apex_core::access::AccessDescriptor) (component
    /// `TypeId`s + a `ScriptVm` write token that serializes Lua↔Lua), and
    /// registers a NonSend (main-thread) runner per declaration. The runner shares
    /// the VM (`Rc<Lua>`) and context, installs the declared access for the run,
    /// and calls the Lua function each frame.
    ///
    /// A declaration referencing an unregistered component is REFUSED (not
    /// registered) and logged loudly (§0.2a): registering it with an
    /// under-declared access would let the scheduler run it concurrently with a
    /// conflicting system (a data race). Register those components first.
    ///
    /// The monolithic [`run`](Self::run) remains the fallback for scripts that
    /// define a top-level `run()` instead of `system{}` declarations.
    ///
    /// Idempotent: calling it again (e.g. after a hot-reload) first REMOVES the
    /// previously-registered script systems from the scheduler, then re-registers
    /// from the current declarations — so a reloaded script's changed `system{}`
    /// set replaces the old one cleanly (no duplicate/stale systems).
    pub fn register_systems(&mut self, scheduler: &mut apex_scheduler::Scheduler) {
        // Remove the previous generation of script systems (hot-reload safety).
        for id in std::mem::take(&mut self.registered_system_ids) {
            scheduler.remove_system(id);
        }

        // Run the active script's chunk so its top-level `system{}` calls record
        // their declarations on the context. (No world access here — `system{}`
        // only records name/query/fn.)
        if self.active_script.is_empty() {
            return;
        }
        let chunk_key = match self.scripts.get(&self.active_script) {
            Some(s) => &s.chunk_key,
            None => {
                log::warn!("register_systems: active script '{}' not found", self.active_script);
                return;
            }
        };
        match self.lua.registry_value::<mlua::Function>(chunk_key) {
            Ok(chunk) => {
                if let Err(e) = chunk.call::<()>(()) {
                    log::error!(
                        "register_systems: error running chunk '{}': {}",
                        self.active_script, e
                    );
                }
            }
            Err(e) => {
                log::error!(
                    "register_systems: failed to get chunk '{}': {}",
                    self.active_script, e
                );
                return;
            }
        }

        let decls = self.ctx.borrow_mut().take_script_systems();
        for decl in decls {
            // Translate the query into scheduler access + declared component ids.
            let access = {
                let ctx = self.ctx.borrow();
                crate::registrar::descs_to_access(&ctx.bindings, &decl.descs)
            };
            if !access.unresolved.is_empty() {
                log::error!(
                    "register_systems: script system '{}' references unregistered \
                     component(s) {:?} — REFUSED (register the component(s) first; an \
                     under-declared access would be a data race under parallelism)",
                    decl.name, access.unresolved
                );
                // Free the function's registry slot — the system will not run.
                let _ = self.lua.remove_registry_value(decl.fn_key);
                continue;
            }

            let lua = self.lua.clone();
            let ctx = self.ctx.clone();
            let fn_key = decl.fn_key;
            let read_ids = access.read_ids;
            let write_ids = access.write_ids;
            let sys_name = decl.name.clone();

            let sys_id = scheduler.add_dynamic_nonsend_system(
                decl.name,
                access.descriptor,
                move |sys_ctx: apex_core::world::SystemContext<'_>| {
                    // The runtime-declared runner: install the world (SHARED) and
                    // the declared access, run the Lua fn (its query/commit go
                    // through the declared-cell path), then clean up.
                    let world = sys_ctx.world_ref();
                    {
                        let mut c = ctx.borrow_mut();
                        // SAFETY: the pointer set from `&World` is read back ONLY as
                        // `&World` (the query/commit declared-cell path) — never as
                        // `&mut World`; the parallel stage holds only `&World`.
                        unsafe { c.set_world_ptr_shared(world); }
                        c.set_declared_access(read_ids.clone(), write_ids.clone());
                    }

                    let result = match lua.registry_value::<mlua::Function>(&fn_key) {
                        Ok(f) => f.call::<()>(()),
                        Err(e) => {
                            log::error!("script system '{}': function missing: {}", sys_name, e);
                            Ok(())
                        }
                    };
                    if let Err(e) = result {
                        log::error!("script system '{}' runtime error: {}", sys_name, e);
                    }

                    {
                        let mut c = ctx.borrow_mut();
                        // Spawn/despawn drain into THIS system's per-system Commands
                        // slot — deterministic ids (D8b reserver), applied by the
                        // scheduler after the stage, never via `&mut World`.
                        c.apply_structural_to_commands(&lua, sys_ctx.commands());
                        // Resource-write/event ops need `&mut World`, unavailable to
                        // a concurrent script system — drop them loudly (§0.2a).
                        if c.discard_deferred_globals(&lua) {
                            log::warn!(
                                "script system '{}': resource-write / event ops are not \
                                 applied from a script SYSTEM — dropped this frame (use a \
                                 Rust system or the monolithic run())",
                                sys_name
                            );
                        }
                        c.clear_declared_access();
                        c.clear_world_ptr();
                    }
                },
            );
            self.registered_system_ids.push(sys_id);
        }
    }

    // ── Hot reload ─────────────────────────────────────────────

    pub fn poll_hot_reload(&mut self) {
        let rx = match &self.watch_rx {
            Some(rx) => rx,
            None     => return,
        };

        let mut changed_paths: Vec<PathBuf> = Vec::new();

        loop {
            match rx.try_recv() {
                Ok(Ok(event)) => {
                    if is_lua_modify_event(&event) {
                        changed_paths.extend(event.paths);
                    }
                }
                Ok(Err(e)) => {
                    log::warn!("ScriptEngine watcher error: {}", e);
                }
                Err(mpsc::TryRecvError::Empty)        => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    log::warn!("ScriptEngine: watcher disconnected");
                    break;
                }
            }
        }

        changed_paths.sort();
        changed_paths.dedup();

        let now = Instant::now();
        let debounce = Duration::from_millis(50);

        for path in &changed_paths {
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None    => continue,
            };

            if let Some(last) = self.last_reload.get(&name) {
                if now.duration_since(*last) < debounce {
                    continue;
                }
            }

            self.reload_file(path);
            self.last_reload.insert(name, now);
        }
    }

    fn reload_file(&mut self, path: &Path) {
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None    => return,
        };

        if !self.scripts.contains_key(&name) {
            log::info!("ScriptEngine: new script '{}'", name);
        } else {
            log::info!("ScriptEngine: reloading script '{}'", name);
        }

        match self.compile_file(path) {
            Ok((chunk_key, env_key)) => {
                self.scripts.insert(name, CompiledScript { chunk_key, env_key, path: path.to_path_buf() });
            }
            Err(e) => {
                log::error!("ScriptEngine: recompilation error '{}': {}", name, e);
            }
        }
    }

    // ── Helpers ───────────────────────────────────────────────

    fn compile_file(&self, path: &Path) -> Result<(mlua::RegistryKey, mlua::RegistryKey), ScriptError> {
        let code = std::fs::read_to_string(path)
            .map_err(|e| ScriptError::io(path.to_string_lossy(), e))?;

        let name = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?");

        let env = self.make_sandbox_env()
            .map_err(|e| ScriptError::compile(name, e))?;
        let env_key = self.lua.create_registry_value(mlua::Value::Table(env))
            .map_err(|e| ScriptError::runtime(name, e))?;
        let env: mlua::Table = self.lua.registry_value(&env_key)
            .map_err(|e| ScriptError::runtime(name, e))?;

        let chunk = self.lua.load(&code)
            .set_name(name)
            .set_environment(env)
            .into_function()
            .map_err(|e| ScriptError::compile(name, e))?;

        let chunk_key = self.lua.create_registry_value(chunk)
            .map_err(|e| ScriptError::runtime(name, e))?;

        Ok((chunk_key, env_key))
    }

    pub fn script_names(&self) -> impl Iterator<Item = &str> {
        self.scripts.keys().map(|s| s.as_str())
    }

    pub fn active_script(&self) -> &str {
        &self.active_script
    }

    pub fn has_scripts(&self) -> bool {
        !self.scripts.is_empty()
    }

    /// Enable/disable auto-commit in query iterators.
    /// When enabled, `commit(entity)` is called automatically when moving
    /// to the next entity in the `for entity in query(...) do ... end` loop.
    pub fn set_auto_commit(&mut self, enabled: bool) {
        self.ctx.borrow_mut().auto_commit = enabled;
    }
}

impl Default for ScriptEngine {
    fn default() -> Self { Self::new() }
}

// ── Helper functions ───────────────────────────────────────────

fn is_lua_modify_event(event: &Event) -> bool {
    match event.kind {
        EventKind::Modify(_) | EventKind::Create(_) => {
            event.paths.iter().any(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("lua")
            })
        }
        _ => false,
    }
}
