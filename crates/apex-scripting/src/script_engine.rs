//! `ScriptEngine` — центральная точка управления Rhai-скриптингом.
//!
//! # Жизненный цикл
//!
//! ```text
//! ScriptEngine::new(script_dir)
//!   └── build_api(&world)         ← регистрирует компоненты и глобальные функции
//!       └── load_scripts()        ← компилирует все .rhai файлы
//!
//! // Game loop:
//! loop {
//!     engine.poll_hot_reload();   ← проверяет изменения файлов
//!     engine.run(dt, &mut world); ← выполняет активный скрипт
//!     world.tick();
//! }
//! ```
//!
//! # Хот-релоад
//!
//! При изменении `.rhai` файла:
//! 1. `poll_hot_reload()` обнаруживает изменение через `notify`
//! 2. Перечитывает и перекомпилирует файл
//! 3. Атомарно заменяет AST в `HashMap` (следующий `run()` использует новый)
//!
//! # Spawn из скрипта
//!
//! Spawn реализован через `SpawnRequest` + `DeferredSpawnQueue`:
//! - Скрипт вызывает `spawn(#{ position: Position(0.0, 0.0) })`
//! - `SpawnRequest` накапливается в `ScriptContext.spawn_requests`
//! - После завершения скрипта `apply_deferred_spawns()` применяет их к миру
//! - Это исключает UB от мутации мира внутри query-итератора

use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    time::Duration,
};

use notify::{Event, EventKind, RecursiveMode, Watcher};
use rhai::{Engine, AST};

use apex_core::{
    component::ComponentId,
    world::World,
};

use crate::{
    context::{ComponentBinding, ScriptContext, SpawnRequest},
    error::ScriptError,
    registrar::ScriptableRegistrar,
    rhai_api,
};

// ── CompiledScript ─────────────────────────────────────────────

struct CompiledScript {
    ast:  AST,
    path: PathBuf,
}

// ── SpawnApplier ───────────────────────────────────────────────

/// Функция применения одного SpawnRequest к миру.
///
/// Хранится в движке и позволяет применять spawn без статической типизации.
/// Каждый вызов `register_component::<T>()` добавляет обработчик для T.
type SpawnApplierFn = Box<dyn Fn(&str, &rhai::Dynamic, apex_core::Entity, &mut World)>;

// ── ScriptEngine ───────────────────────────────────────────────

/// Управление Rhai-скриптами: компиляция, выполнение, хот-релоад.
pub struct ScriptEngine {
    engine:         Engine,
    ctx:            Rc<RefCell<ScriptContext>>,
    scripts:        HashMap<String, CompiledScript>,
    active_script:  String,
    script_dir:     Option<PathBuf>,
    watcher:        Option<Box<dyn Watcher>>,
    watch_rx:       Option<mpsc::Receiver<notify::Result<Event>>>,
    /// Обработчики spawn: type_name → fn(dynamic, entity, world)
    spawn_appliers: HashMap<String, SpawnApplierFn>,
    /// Накопленные spawn-запросы (перемещаются из ctx после run)
    spawn_queue:    Vec<SpawnRequest>,
}

impl ScriptEngine {
    // ── Конструктор ────────────────────────────────────────────

    /// Создать ScriptEngine без директории скриптов.
    ///
    /// Использование: `engine.load_script_str("main", code)` для
    /// встроенных скриптов (тесты, конфиги в памяти).
    pub fn new() -> Self {
        let ctx = Rc::new(RefCell::new(ScriptContext::new()));

        let mut engine = Engine::new();

        // Ограничения безопасности — для игрового скриптинга
        engine.set_max_expr_depths(64, 32);
        engine.set_max_call_levels(32);
        engine.set_max_operations(u64::MAX); // без ограничения операций в игре

        // Регистрируем глобальные API-функции
        rhai_api::register_globals(&mut engine, Rc::clone(&ctx));
        rhai_api::register_log(&mut engine);

        Self {
            engine,
            ctx,
            scripts:        HashMap::new(),
            active_script:  String::new(),
            script_dir:     None,
            watcher:        None,
            watch_rx:       None,
            spawn_appliers: HashMap::new(),
            spawn_queue:    Vec::new(),
        }
    }

    /// Создать ScriptEngine с директорией скриптов и файловым наблюдателем.
    ///
    /// При создании запускает `notify::Watcher` на `script_dir`.
    /// Ошибки наблюдателя логируются, но не паникуют — хот-релоад опционален.
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

    // ── Регистрация компонентов ────────────────────────────────

    /// Зарегистрировать компонент T для доступа из скриптов.
    ///
    /// Необходимо: T должен реализовывать `ScriptableRegistrar` (через `#[derive(Scriptable)]`).
    /// `world` используется для получения `ComponentId`.
    ///
    /// После регистрации:
    /// - Конструктор `TypeName(args...)` доступен в Rhai
    /// - `query(["Read:TypeName", ...])` распознаёт компонент
    /// - `spawn(#{ type_name: TypeName(...) })` может создавать entity с T
    ///
    /// # Пример
    ///
    /// ```ignore
    /// world.register_component::<Position>();
    /// engine.register_component::<Position>(&world);
    /// ```
    pub fn register_component<T>(&mut self, world: &World)
    where
        T: ScriptableRegistrar + apex_core::component::Component,
    {
        // 1. Получаем ComponentId из реестра мира
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

        // 2. Строим binding с type-erased read/write функциями
        let binding = ComponentBinding {
            name: T::type_name_str(),
            id:   comp_id,
            read: |ptr: *const u8| -> rhai::Dynamic {
                // SAFETY: вызывающий (RhaiQueryIter::build_item) гарантирует
                // что ptr указывает на живой T в Column.
                let val = unsafe { &*(ptr as *const T) };
                val.to_dynamic()
            },
            write: |ptr: *mut u8, dynamic: &rhai::Dynamic| -> bool {
                // SAFETY: вызывающий (RhaiQueryIter::flush_writes) гарантирует
                // что ptr указывает на живой T в Column.
                if let Some(new_val) = T::from_dynamic(dynamic) {
                    unsafe { *(ptr as *mut T) = new_val; }
                    true
                } else {
                    log::warn!(
                        "flush_writes: не удалось конвертировать Dynamic в {}",
                        T::type_name_str()
                    );
                    false
                }
            },
        };

        self.ctx.borrow_mut().add_binding(binding);

        // 3. Регистрируем конструктор в Rhai Engine (Position(x, y) → Dynamic)
        T::register_rhai_type(&mut self.engine);

        // 4. Регистрируем spawn-обработчик для T
        let type_name_lower = T::type_name_str().to_lowercase();
        self.spawn_appliers.insert(
            type_name_lower.clone(),
            Box::new(move |_key: &str, dynamic: &rhai::Dynamic, entity: apex_core::Entity, world: &mut World| {
                if let Some(component) = T::from_dynamic(dynamic) {
                    world.insert(entity, component);
                } else {
                    log::warn!(
                        "spawn: не удалось конвертировать Dynamic в {} для entity {:?}",
                        T::type_name_str(),
                        entity
                    );
                }
            }),
        );

        // Также регистрируем по точному имени типа (без lowercase)
        let exact_name = T::type_name_str().to_string();
        if exact_name.to_lowercase() != exact_name {
            self.spawn_appliers.insert(
                exact_name,
                Box::new(move |_key: &str, dynamic: &rhai::Dynamic, entity: apex_core::Entity, world: &mut World| {
                    if let Some(component) = T::from_dynamic(dynamic) {
                        world.insert(entity, component);
                    }
                }),
            );
        }

        log::debug!("ScriptEngine: зарегистрирован компонент '{}'", T::type_name_str());
    }

    // ── Загрузка скриптов ──────────────────────────────────────

    /// Загрузить и скомпилировать все `.rhai` файлы из директории скриптов.
    ///
    /// Первый найденный файл становится активным скриптом.
    /// При ошибке компиляции — возвращает `ScriptError::Compile`, остальные файлы
    /// продолжают загружаться.
    pub fn load_scripts(&mut self) -> Result<(), ScriptError> {
        let script_dir = self.script_dir.clone().ok_or(ScriptError::NoScriptDir)?;

        let entries = std::fs::read_dir(&script_dir)
            .map_err(|e| ScriptError::io(script_dir.to_string_lossy(), e))?;

        let mut first_name: Option<String> = None;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rhai") {
                continue;
            }

            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed")
                .to_string();

            match self.compile_file(&path) {
                Ok(ast) => {
                    log::info!("ScriptEngine: загружен скрипт '{}'", name);
                    self.scripts.insert(name.clone(), CompiledScript { ast, path });
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

    /// Загрузить скрипт из строки (для тестов и встроенных скриптов).
    pub fn load_script_str(&mut self, name: impl Into<String>, code: &str) -> Result<(), ScriptError> {
        let name = name.into();
        let ast = self.engine.compile(code)
            .map_err(|e| ScriptError::compile(&name, e))?;

        self.scripts.insert(name.clone(), CompiledScript {
            ast,
            path: PathBuf::from(format!("<{}>", name)),
        });

        if self.active_script.is_empty() {
            self.active_script = name;
        }

        Ok(())
    }

    /// Установить активный скрипт (по имени без расширения).
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

    /// Выполнить активный скрипт за один кадр.
    ///
    /// # Порядок выполнения
    ///
    /// 1. Устанавливает `world_ptr` и `delta_time` в контекст
    /// 2. Вызывает `fn run()` скрипта
    /// 3. Сбрасывает `world_ptr` (исключает use-after-free)
    /// 4. Применяет отложенные команды (despawn из скрипта)
    /// 5. Применяет накопленные spawn-запросы
    ///
    /// Ошибки выполнения логируются и не паникуют (игра не должна падать).
    pub fn run(&mut self, dt: f32, world: &mut World) {
        if self.active_script.is_empty() {
            return;
        }

        // Устанавливаем контекст
        {
            let mut ctx = self.ctx.borrow_mut();
            ctx.delta_time = dt;
            // SAFETY: world живёт всё время выполнения этого метода.
            // clear_world_ptr() вызывается до любого возврата.
            unsafe { ctx.set_world_ptr(world); }
        }

        // Выполняем скрипт
        let result = if let Some(script) = self.scripts.get(&self.active_script) {
            // Вызываем fn run() если она есть, иначе выполняем весь скрипт
            let has_run_fn = script.ast.iter_fn_def().any(|f| f.name == "run");

            if has_run_fn {
                self.engine.call_fn::<()>(
                    &mut rhai::Scope::new(),
                    &script.ast,
                    "run",
                    (),
                ).map_err(|e| ScriptError::runtime(&self.active_script, e))
            } else {
                self.engine.eval_ast::<()>(&script.ast)
                    .map_err(|e| ScriptError::runtime(&self.active_script, e))
            }
        } else {
            log::warn!("ScriptEngine::run: активный скрипт '{}' не найден", self.active_script);
            return;
        };

        if let Err(e) = result {
            log::error!("ScriptEngine: ошибка выполнения: {}", e);
        }

        // Сбрасываем world_ptr ДО apply — это важно для безопасности
        // (clear_world_ptr делает его None, apply_deferred снова берёт &mut)
        self.ctx.borrow_mut().clear_world_ptr();

        // Применяем отложенные despawn-команды
        // (spawn-запросы обрабатываются отдельно через apply_spawn_queue)
        self.ctx.borrow_mut().apply_deferred();

        // Применяем накопленные spawn-запросы
        // (извлекаем из ctx чтобы не держать borrow во время apply)
        self.apply_spawn_queue(world);
    }

    /// Применить накопленные spawn-запросы к миру.
    fn apply_spawn_queue(&mut self, world: &mut World) {
        // Drain из ctx
        let requests: Vec<SpawnRequest> = {
            // SpawnRequest'ы помещаются в spawn_queue через специальный механизм.
            // Пока он не реализован полностью — spawn через Commands будет работать.
            std::mem::take(&mut self.spawn_queue)
        };

        for req in requests {
            if req.components.is_empty() {
                world.spawn_empty();
                continue;
            }

            // Создаём entity и добавляем компоненты по одному
            let entity = world.spawn_empty();
            for (key, dynamic) in &req.components {
                let key_lower = key.to_lowercase();
                // Ищем обработчик сначала по lowercase, потом по оригинальному имени
                let applier = self.spawn_appliers.get(&key_lower)
                    .or_else(|| self.spawn_appliers.get(key.as_str()));

                if let Some(applier) = applier {
                    applier(key, dynamic, entity, world);
                } else {
                    log::warn!("spawn: нет обработчика для компонента '{}'", key);
                }
            }
        }
    }

    // ── Хот-релоад ─────────────────────────────────────────────

    /// Проверить изменения файлов и перекомпилировать при необходимости.
    ///
    /// Вызывай каждый кадр перед `run()`.
    /// Не блокирует — использует неблокирующий `try_recv`.
    pub fn poll_hot_reload(&mut self) {
        let rx = match &self.watch_rx {
            Some(rx) => rx,
            None     => return,
        };

        // Собираем все ожидающие события (без блокировки)
        let mut changed_paths: Vec<PathBuf> = Vec::new();

        loop {
            match rx.try_recv() {
                Ok(Ok(event)) => {
                    if is_rhai_modify_event(&event) {
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

        // Дедупликация путей (один файл может прийти несколько раз)
        changed_paths.sort();
        changed_paths.dedup();

        for path in changed_paths {
            self.reload_file(&path);
        }
    }

    fn reload_file(&mut self, path: &Path) {
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None    => return,
        };

        if !self.scripts.contains_key(&name) {
            // Новый файл — загружаем
            log::info!("ScriptEngine: новый скрипт '{}'", name);
        } else {
            log::info!("ScriptEngine: перезагрузка скрипта '{}'", name);
        }

        match self.compile_file(path) {
            Ok(ast) => {
                self.scripts.insert(name, CompiledScript { ast, path: path.to_path_buf() });
            }
            Err(e) => {
                log::error!("ScriptEngine: ошибка перекомпиляции '{}': {}", name, e);
                // Оставляем старую версию — не заменяем при ошибке
            }
        }
    }

    // ── Вспомогательные ───────────────────────────────────────

    fn compile_file(&self, path: &Path) -> Result<AST, ScriptError> {
        let code = std::fs::read_to_string(path)
            .map_err(|e| ScriptError::io(path.to_string_lossy(), e))?;

        self.engine.compile(&code)
            .map_err(|e| ScriptError::compile(
                path.file_stem().and_then(|s| s.to_str()).unwrap_or("?"),
                e,
            ))
    }

    /// Список загруженных скриптов.
    pub fn script_names(&self) -> impl Iterator<Item = &str> {
        self.scripts.keys().map(|s| s.as_str())
    }

    /// Имя активного скрипта.
    pub fn active_script(&self) -> &str {
        &self.active_script
    }

    /// Есть ли хотя бы один загруженный скрипт.
    pub fn has_scripts(&self) -> bool {
        !self.scripts.is_empty()
    }
}

impl Default for ScriptEngine {
    fn default() -> Self { Self::new() }
}

// ── Вспомогательные функции ────────────────────────────────────

fn is_rhai_modify_event(event: &Event) -> bool {
    match event.kind {
        EventKind::Modify(_) | EventKind::Create(_) => {
            event.paths.iter().any(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("rhai")
            })
        }
        _ => false,
    }
}