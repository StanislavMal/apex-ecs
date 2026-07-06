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

use apex_core::world::World;

use crate::{
    context::{ComponentBinding, ScriptContext, SpawnRequest},
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

// ── SpawnApplier ───────────────────────────────────────────────

type SpawnApplierFn = Box<dyn Fn(&str, &mlua::Value, apex_core::Entity, &mut World)>;

// ── ScriptEngine ───────────────────────────────────────────────

pub struct ScriptEngine {
    lua:            mlua::Lua,
    ctx:            Rc<RefCell<ScriptContext>>,
    scripts:        HashMap<String, CompiledScript>,
    active_script:  String,
    script_dir:     Option<PathBuf>,
    watcher:        Option<Box<dyn Watcher>>,
    watch_rx:       Option<mpsc::Receiver<notify::Result<Event>>>,
    last_reload:    HashMap<String, Instant>,
    spawn_appliers: HashMap<String, SpawnApplierFn>,
    registered_components: Vec<String>,
}

impl ScriptEngine {
    // ── Constructor ────────────────────────────────────────────

    pub fn new() -> Self {
        let lua = mlua::Lua::new();

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
            spawn_appliers: HashMap::new(),
            registered_components: Vec::new(),
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
        for name in &["delta_time", "entity_count", "query", "commit",
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

        let type_name_lower = T::type_name_str().to_lowercase();
        self.spawn_appliers.insert(
            type_name_lower.clone(),
            Box::new(move |_key: &str, val: &mlua::Value, entity: apex_core::Entity, world: &mut World| {
                if let Some(component) = T::from_lua(val) {
                    world.insert(entity, component);
                } else {
                    log::warn!("spawn: failed to convert Lua value to {}", T::type_name_str());
                }
            }),
        );

        let exact_name = T::type_name_str().to_string();
        if exact_name.to_lowercase() != exact_name {
            self.spawn_appliers.insert(
                exact_name,
                Box::new(move |_key: &str, val: &mlua::Value, entity: apex_core::Entity, world: &mut World| {
                    if let Some(component) = T::from_lua(val) {
                        world.insert(entity, component);
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

        self.ctx.borrow_mut().apply_deferred();

        {
            let mut ctx = self.ctx.borrow_mut();
            ctx.apply_deferred_resources_and_events(&self.lua);
        }

        self.ctx.borrow_mut().clear_world_ptr();

        self.apply_spawn_queue(world);
    }

    fn apply_spawn_queue(&mut self, world: &mut World) {
        let requests: Vec<SpawnRequest> = {
            let mut ctx = self.ctx.borrow_mut();
            std::mem::take(&mut ctx.deferred_spawns)
        };

        for req in requests {
            if req.components.is_empty() {
                world.spawn(());
                continue;
            }

            let entity = world.spawn(());

            let appliers: Vec<(String, Option<&SpawnApplierFn>)> = req.components.iter()
                .map(|(key, _)| {
                    let key_lower = key.to_lowercase();
                    let key_no_underscore: String = key_lower.chars().filter(|c| *c != '_').collect();
                    let applier = self.spawn_appliers.get(&key_lower)
                        .or_else(|| self.spawn_appliers.get(&key_no_underscore))
                        .or_else(|| self.spawn_appliers.get(key.as_str()));
                    (key.clone(), applier)
                })
                .collect();

            for ((key, reg_key), (_, applier)) in req.components.into_iter().zip(appliers.iter()) {
                if let Some(applier) = applier {
                    let val: mlua::Value = match self.lua.registry_value(&reg_key) {
                        Ok(v) => v,
                        Err(e) => {
                            log::warn!("spawn: failed to extract value for '{}': {}", key, e);
                            continue;
                        }
                    };
                    applier(&key, &val, entity, world);
                    let _ = self.lua.remove_registry_value(reg_key);
                } else {
                    log::warn!("spawn: no handler for component '{}'", key);
                }
            }
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
