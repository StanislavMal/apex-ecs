//! `ScriptEngine` — центральная точка управления Lua-скриптингом.
//!
//! # Жизненный цикл
//!
//! ```text
//! ScriptEngine::new()
//!   └── lua.globals().set(...)    ← регистрация API функций
//!   └── register_component::<T>() ← регистрация компонентов
//!       └── load_scripts()        ← компилирует все .lua файлы
//!
//! // Game loop:
//! loop {
//!     engine.poll_hot_reload();   ← проверяет изменения файлов
//!     engine.run(dt, &mut world); ← выполняет активный скрипт
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
    /// Sandbox-окружение (_ENV) для изоляции скрипта
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
    // ── Конструктор ────────────────────────────────────────────

    pub fn new() -> Self {
        let lua = mlua::Lua::new();

        let ctx = Rc::new(RefCell::new(ScriptContext::new()));

        if let Err(e) = lua_api::register_globals(&lua) {
            log::error!("ScriptEngine: ошибка регистрации Lua API: {}", e);
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
                    log::warn!("ScriptEngine: не удалось установить watcher на {:?}: {}", script_dir, e);
                } else {
                    log::debug!("ScriptEngine: наблюдение за {:?}", script_dir);
                    this.watcher  = Some(Box::new(w));
                    this.watch_rx = Some(rx);
                }
            }
            Err(e) => {
                log::warn!("ScriptEngine: не удалось создать watcher: {}", e);
            }
        }

        this.script_dir = Some(script_dir.to_path_buf());
        this
    }

    // ── Sandbox ─────────────────────────────────────────────────

    /// Создать изолированное окружение для скрипта.
    /// Включает стандартные библиотеки Lua, API-функции и конструкторы компонентов.
    fn make_sandbox_env(&self) -> mlua::Result<mlua::Table> {
        let env = self.lua.create_table()?;

        // Стандартные библиотеки
        for name in &["math", "string", "table", "ipairs", "pairs", "next",
                      "select", "tonumber", "tostring", "type", "unpack"] {
            if let Ok(val) = self.lua.globals().get::<mlua::Value>(*name) {
                env.set(*name, val)?;
            }
        }

        // API-функции
        for name in &["delta_time", "entity_count", "query", "commit",
                      "spawn_entity", "despawn", "read_resource", "write_resource",
                      "emit_event", "log", "print", "log_debug", "log_warn", "log_error",
                      "inspect"] {
            if let Ok(val) = self.lua.globals().get::<mlua::Value>(*name) {
                env.set(*name, val)?;
            }
        }

        // Конструкторы компонентов (Position.new, Velocity.new, ...)
        for name in &self.registered_components {
            if let Ok(val) = self.lua.globals().get::<mlua::Value>(name.as_str()) {
                env.set(name.as_str(), val)?;
            }
        }

        Ok(env)
    }

    // ── Регистрация компонентов ────────────────────────────────

    pub fn register_component<T>(&mut self, world: &World)
    where
        T: ScriptableRegistrar + apex_core::component::Component,
    {
        let comp_id = match world.registry().get_id::<T>() {
            Some(id) => id,
            None => {
                log::warn!(
                    "ScriptEngine::register_component: {} не зарегистрирован в World. \
                     Вызови world.register_component::<{}>() сначала.",
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
                    log::warn!("commit: не удалось конвертировать Lua value в {}", T::type_name_str());
                    false
                }
            },
        };

        self.ctx.borrow_mut().add_binding(binding);

        if let Err(e) = T::register_lua_type(&self.lua) {
            log::error!("ScriptEngine: ошибка регистрации {}: {}", T::type_name_str(), e);
        }

        self.registered_components.push(T::type_name_str().to_string());

        let type_name_lower = T::type_name_str().to_lowercase();
        self.spawn_appliers.insert(
            type_name_lower.clone(),
            Box::new(move |_key: &str, val: &mlua::Value, entity: apex_core::Entity, world: &mut World| {
                if let Some(component) = T::from_lua(val) {
                    world.insert(entity, component);
                } else {
                    log::warn!("spawn: не удалось конвертировать Lua value в {}", T::type_name_str());
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

        log::debug!("ScriptEngine: зарегистрирован компонент '{}'", T::type_name_str());
    }

    // ── Регистрация ресурсов ───────────────────────────────────

    pub fn register_resource<T>(&mut self)
    where
        T: ScriptableRegistrar + Send + Sync,
    {
        let binding = ResourceBinding {
            name: T::type_name_str(),
            read: |lua: &mlua::Lua, world: &World| -> mlua::Result<mlua::Value> {
                let res = world.resources.try_get::<T>()
                    .ok_or_else(|| mlua::Error::runtime(format!("resource '{}' not found", T::type_name_str())))?;
                res.to_lua(lua)
            },
            write: |value: &mlua::Value, world: &mut World| -> bool {
                if let Some(new_val) = T::from_lua(value) {
                    world.resources.insert(new_val);
                    true
                } else {
                    log::warn!("write_resource: не удалось конвертировать Lua value в {}", T::type_name_str());
                    false
                }
            },
        };
        self.ctx.borrow_mut().add_resource_binding(binding);
        if let Err(e) = T::register_lua_type(&self.lua) {
            log::error!("ScriptEngine: ошибка регистрации конструктора {}: {}", T::type_name_str(), e);
        }
        self.registered_components.push(T::type_name_str().to_string());
        log::debug!("ScriptEngine: зарегистрирован ресурс '{}'", T::type_name_str());
    }

    // ── Регистрация событий ────────────────────────────────────

    pub fn register_event<T>(&mut self)
    where
        T: ScriptableRegistrar + Send + Sync,
    {
        let binding = EventBinding {
            name: T::type_name_str(),
            emit: |value: &mlua::Value, world: &mut World| -> bool {
                if let Some(event) = T::from_lua(value) {
                    // send_event авторегистрирует тип — отправка всегда успешна.
                    world.send_event(event);
                    true
                } else {
                    log::warn!("emit_event: не удалось конвертировать Lua value в {}", T::type_name_str());
                    false
                }
            },
        };
        self.ctx.borrow_mut().add_event_binding(binding);
        if let Err(e) = T::register_lua_type(&self.lua) {
            log::error!("ScriptEngine: ошибка регистрации конструктора {}: {}", T::type_name_str(), e);
        }
        self.registered_components.push(T::type_name_str().to_string());
        log::debug!("ScriptEngine: зарегистрировано событие '{}'", T::type_name_str());
    }

    // ── Загрузка скриптов ──────────────────────────────────────

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
                    log::info!("ScriptEngine: загружен скрипт '{}'", name);
                    self.scripts.insert(name.clone(), CompiledScript { chunk_key, env_key, path });
                    if first_name.is_none() {
                        first_name = Some(name);
                    }
                }
                Err(e) => {
                    log::error!("ScriptEngine: ошибка компиляции '{}': {}", name, e);
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

        // Пересоздаём env из registry — set_environment забирает владение
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

    // ── Выполнение ─────────────────────────────────────────────

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
                    log::error!("ScriptEngine: не удалось получить chunk '{}': {}", self.active_script, e);
                    self.ctx.borrow_mut().clear_world_ptr();
                    return;
                }
            };

            // Выполняем чанк — определяет функции в sandbox _ENV
            if let Err(e) = chunk.call::<()>(()) {
                log::error!("ScriptEngine: ошибка выполнения '{}': {}", self.active_script, e);
            }

            // Вызываем run() из sandbox-окружения
            let env: mlua::Table = match self.lua.registry_value(&script.env_key) {
                Ok(t) => t,
                Err(e) => {
                    log::error!("ScriptEngine: не удалось получить sandbox '{}': {}", self.active_script, e);
                    self.ctx.borrow_mut().clear_world_ptr();
                    return;
                }
            };
            match env.get::<mlua::Function>("run") {
                Ok(run_fn) => {
                    if let Err(e) = run_fn.call::<()>(()) {
                        log::error!("ScriptEngine: ошибка в run() '{}': {}", self.active_script, e);
                    }
                }
                Err(_) => {
                    // run() не определена — скрипт без функции, только top-level код (уже выполнен)
                }
            }
        } else {
            log::warn!("ScriptEngine::run: активный скрипт '{}' не найден", self.active_script);
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
                            log::warn!("spawn: не удалось извлечь значение для '{}': {}", key, e);
                            continue;
                        }
                    };
                    applier(&key, &val, entity, world);
                    let _ = self.lua.remove_registry_value(reg_key);
                } else {
                    log::warn!("spawn: нет обработчика для компонента '{}'", key);
                }
            }
        }
    }

    // ── Хот-релоад ─────────────────────────────────────────────

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
                    log::warn!("ScriptEngine: watcher отключён");
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
            log::info!("ScriptEngine: новый скрипт '{}'", name);
        } else {
            log::info!("ScriptEngine: перезагрузка скрипта '{}'", name);
        }

        match self.compile_file(path) {
            Ok((chunk_key, env_key)) => {
                self.scripts.insert(name, CompiledScript { chunk_key, env_key, path: path.to_path_buf() });
            }
            Err(e) => {
                log::error!("ScriptEngine: ошибка перекомпиляции '{}': {}", name, e);
            }
        }
    }

    // ── Вспомогательные ───────────────────────────────────────

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

    /// Включить/выключить авто-commit в query-итераторах.
    /// Когда включено, `commit(entity)` вызывается автоматически при переходе
    /// к следующей entity в цикле `for entity in query(...) do ... end`.
    pub fn set_auto_commit(&mut self, enabled: bool) {
        self.ctx.borrow_mut().auto_commit = enabled;
    }
}

impl Default for ScriptEngine {
    fn default() -> Self { Self::new() }
}

// ── Вспомогательные функции ────────────────────────────────────

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
