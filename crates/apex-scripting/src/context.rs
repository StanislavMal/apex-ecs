//! `ScriptContext` — разделяемый контекст между Rust и Rhai в пределах одного `run()`.
//!
//! # Lifetime и безопасность
//!
//! `ScriptContext` живёт ровно столько, сколько `ScriptEngine::run()`.
//! `world_ptr` устанавливается перед вызовом скрипта и сбрасывается (`null`) сразу после.
//! Таким образом:
//! - Всё время выполнения скрипта ptr валиден
//! - Сохранить `ScriptContext` в статике и использовать после `run()` — невозможно
//!   без `unsafe`, что явно сигнализирует об ошибке
//!
//! # Отложенные изменения
//!
//! Rhai-итератор удерживает shared borrow на World через ptr, поэтому
//! структурные изменения (spawn/despawn) нельзя применять внутри итерации.
//! Они накапливаются в `deferred: Commands` и применяются после завершения скрипта.

use std::{
    cell::RefCell,
    collections::HashMap,
    ptr::NonNull,
};

use apex_core::{
    commands::Commands,
    component::ComponentId,
    world::World,
};

use crate::registrar::{ResourceBinding, EventBinding, ScriptableRegistrar};

// ── ComponentBinding ───────────────────────────────────────────

/// Информация о компоненте зарегистрированном для скриптинга.
///
/// Хранит функции конвертации компонент ↔ Dynamic без привязки к конкретному типу T.
pub struct ComponentBinding {
    /// Имя типа компонента (совпадает с ключом в query Map)
    pub name: &'static str,
    /// ComponentId для поиска в архетипах
    pub id:   ComponentId,
    /// Читать компонент из Column[row] → Dynamic
    pub read: unsafe fn(*const u8) -> rhai::Dynamic,
    /// Записать компонент в Column[row] из Dynamic; возвращает false если тип неверен
    pub write: unsafe fn(*mut u8, &rhai::Dynamic) -> bool,
}

// ── SpawnRequest ───────────────────────────────────────────────

/// Запрос на создание entity, сформированный из скрипта.
///
/// Хранит список (name, Dynamic) пар — компоненты для нового entity.
/// Применяется через `apply_deferred()` после завершения итератора.
pub struct SpawnRequest {
    /// Список компонентов: (имя типа, Dynamic Map с данными)
    pub components: Vec<(String, rhai::Dynamic)>,
}

// ── ScriptContext ──────────────────────────────────────────────

/// Мост между Rhai-скриптом и миром ECS.
///
/// Живёт в `Rc<RefCell<ScriptContext>>` — клоны `Rc` захватываются
/// замыканиями зарегистрированными в `rhai::Engine`.
pub struct ScriptContext {
    /// Текущий delta time кадра — устанавливается перед `run()`
    pub delta_time: f32,

    /// Сырой указатель на мир. Живёт ровно в пределах `run()`.
    /// `None` означает что мы вне `run()` — любое обращение через скрипт
    /// вернёт ошибку вместо UB.
    world_ptr: Option<NonNull<World>>,

    /// Буфер отложенных команд spawn/despawn.
    /// Применяется после завершения скрипта через `apply_deferred()`.
    pub(crate) deferred: RefCell<Commands>,

    /// Буфер запросов spawn из скриптов (SpawnRequest содержит rhai::Dynamic,
    /// который не Send, поэтому не может идти через Commands::add).
    /// Применяется в apply_deferred_requests после завершения скрипта.
    pub(crate) deferred_spawns: RefCell<Vec<SpawnRequest>>,

    /// Реестр компонентов доступных из скриптов: name → binding
    pub(crate) bindings: HashMap<&'static str, ComponentBinding>,

    /// Реестр ресурсов доступных из скриптов: name → binding
    pub(crate) resource_bindings: HashMap<&'static str, ResourceBinding>,

    /// Реестр событий доступных из скриптов: name → binding
    pub(crate) event_bindings: HashMap<&'static str, EventBinding>,

    /// Буфер отложенных записей ресурсов: (type_name, Dynamic)
    /// Применяется после завершения скрипта, чтобы избежать RefCell double-borrow
    /// при вызове write_resource() внутри query()-итерации.
    pub(crate) deferred_resource_writes: RefCell<Vec<(String, rhai::Dynamic)>>,

    /// Буфер отложенных событий: (type_name, Dynamic)
    /// Применяется после завершения скрипта, чтобы избежать RefCell double-borrow
    /// при вызове emit_event() внутри query()-итерации.
    pub(crate) deferred_events: RefCell<Vec<(String, rhai::Dynamic)>>,

    /// Счётчик entity — кешируется чтобы не вызывать world через ptr каждый раз
    entity_count_cache: usize,
}

impl ScriptContext {
    pub fn new() -> Self {
        Self {
            delta_time:              0.0,
            world_ptr:               None,
            deferred:                RefCell::new(Commands::new()),
            deferred_spawns:         RefCell::new(Vec::new()),
            bindings:                HashMap::new(),
            resource_bindings:       HashMap::new(),
            event_bindings:          HashMap::new(),
            deferred_resource_writes: RefCell::new(Vec::new()),
            deferred_events:         RefCell::new(Vec::new()),
            entity_count_cache:      0,
        }
    }

    // ── Lifetime management ────────────────────────────────────

    /// Установить указатель на мир перед выполнением скрипта.
    ///
    /// # Safety
    /// Вызывающий обязан гарантировать что `world` живёт не меньше чем
    /// следующий вызов `clear_world_ptr()`.
    pub(crate) unsafe fn set_world_ptr(&mut self, world: &mut World) {
        self.world_ptr         = Some(NonNull::new_unchecked(world as *mut World));
        self.entity_count_cache = world.entity_count();
        self.deferred.borrow_mut().clear();
        self.deferred_resource_writes.borrow_mut().clear();
        self.deferred_events.borrow_mut().clear();
    }

    /// Сбросить указатель на мир после завершения скрипта.
    pub(crate) fn clear_world_ptr(&mut self) {
        self.world_ptr = None;
    }

    /// Получить `&World` — только для чтения (query-итераторы).
    ///
    /// Паника если вызывается вне `run()`.
    pub(crate) fn world_ref(&self) -> &World {
        unsafe {
            self.world_ptr
                .expect("ScriptContext::world_ref вызван вне run()")
                .as_ref()
        }
    }

    /// Получить `&mut World` — для применения deferred команд.
    ///
    /// # Safety
    /// Вызывается ТОЛЬКО из `apply_deferred()` когда итератор точно завершён.
    pub(crate) unsafe fn world_mut(&mut self) -> &mut World {
        self.world_ptr
            .expect("ScriptContext::world_mut вызван вне run()")
            .as_mut()
    }

    // ── API для Rhai-функций ───────────────────────────────────

    /// Текущий delta time кадра.
    pub fn delta_time(&self) -> f32 {
        self.delta_time
    }

    /// Количество живых entity (кешировано на момент начала `run()`).
    pub fn entity_count(&self) -> usize {
        self.entity_count_cache
    }

    /// Поставить в очередь запрос на создание entity.
    pub fn queue_spawn(&self, request: SpawnRequest) {
        // Сохраняем запрос в отдельный буфер — Commands::add требует Send,
        // а SpawnRequest содержит Rc (из rhai::Dynamic). Применение будет
        // выполнено в apply_deferred_requests.
        self.deferred_spawns.borrow_mut().push(request);
    }

    /// Поставить в очередь уничтожение entity.
    pub fn queue_despawn(&self, entity: apex_core::Entity) {
        self.deferred.borrow_mut().despawn(entity);
    }

    /// Применить все накопленные deferred-команды к миру.
    ///
    /// Вызывается `ScriptEngine::run()` после завершения скрипта.
    pub(crate) fn apply_deferred(&mut self) {
        // Извлекаем deferred ДО вызова world_mut, чтобы избежать borrow conflict
        let mut deferred = std::mem::take(&mut *self.deferred.borrow_mut());
        // SAFETY: apply_deferred вызывается только после того как скрипт
        // завершился и никаких borrow на world_ref больше нет.
        let world = unsafe { self.world_mut() };
        deferred.apply(world);
        // Возвращаем очищенный Commands обратно (уже пустой после apply)
        *self.deferred.borrow_mut() = deferred;
    }

    /// Применить отложенные записи ресурсов и отправки событий.
    ///
    /// Вызывается `ScriptEngine::run()` после завершения скрипта,
    /// когда никаких borrow на `ScriptContext` больше нет.
    pub(crate) fn apply_deferred_resources_and_events(&mut self) {
        let writes = std::mem::take(&mut *self.deferred_resource_writes.borrow_mut());
        let events = std::mem::take(&mut *self.deferred_events.borrow_mut());

        if writes.is_empty() && events.is_empty() {
            return;
        }

        // Извлекаем биндинги заранее, чтобы избежать borrow conflict
        // с deferred_resource_writes/events (RefCell)
        let resource_bindings: Vec<(&'static str, fn(&mut World, &rhai::Dynamic) -> bool)> = writes.iter()
            .filter_map(|(name, _)| {
                self.resource_bindings.get(name.as_str())
                    .map(|b| (b.name, b.write))
            })
            .collect();

        let event_bindings: Vec<(&'static str, fn(&mut World, &rhai::Dynamic) -> bool)> = events.iter()
            .filter_map(|(name, _)| {
                self.event_bindings.get(name.as_str())
                    .map(|b| (b.name, b.emit))
            })
            .collect();

        // SAFETY: вызывается после завершения скрипта, никаких borrow нет
        let world = unsafe { self.world_mut() };

        for (type_name, value) in writes {
            if let Some(&(_, write_fn)) = resource_bindings.iter()
                .find(|(name, _)| *name == type_name.as_str())
            {
                write_fn(world, &value);
            }
        }
        for (type_name, value) in events {
            if let Some(&(_, emit_fn)) = event_bindings.iter()
                .find(|(name, _)| *name == type_name.as_str())
            {
                emit_fn(world, &value);
            }
        }
    }

    // ── Регистрация компонентов ────────────────────────────────

    /// Зарегистрировать binding для компонента.
    ///
    /// Вызывается `ScriptEngine::register_component::<T>()`.
    pub(crate) fn add_binding(&mut self, binding: ComponentBinding) {
        self.bindings.insert(binding.name, binding);
    }

    /// Найти binding по имени типа.
    pub(crate) fn binding(&self, name: &str) -> Option<&ComponentBinding> {
        self.bindings.get(name)
    }

    // ── Регистрация ресурсов ───────────────────────────────────

    /// Зарегистрировать binding для ресурса.
    pub(crate) fn add_resource_binding(&mut self, binding: ResourceBinding) {
        self.resource_bindings.insert(binding.name, binding);
    }

    /// Найти binding ресурса по имени типа.
    pub(crate) fn resource_binding(&self, name: &str) -> Option<&ResourceBinding> {
        self.resource_bindings.get(name)
    }

    // ── Регистрация событий ────────────────────────────────────

    /// Зарегистрировать binding для события.
    pub(crate) fn add_event_binding(&mut self, binding: EventBinding) {
        self.event_bindings.insert(binding.name, binding);
    }

    /// Найти binding события по имени типа.
    pub(crate) fn event_binding(&self, name: &str) -> Option<&EventBinding> {
        self.event_bindings.get(name)
    }

    // ── Доступ к ресурсам из Rhai ──────────────────────────────

    /// Прочитать ресурс по имени типа.
    /// Возвращает `None` если ресурс не зарегистрирован.
    pub fn read_resource(&self, type_name: &str) -> Option<rhai::Dynamic> {
        let binding = self.resource_bindings.get(type_name)?;
        let world = self.world_ref();
        (binding.read)(world)
    }

    /// Записать ресурс по имени типа (отложенно).
    ///
    /// Запрос буферизируется и применяется после завершения скрипта,
    /// чтобы избежать RefCell double-borrow при вызове внутри query()-итерации.
    pub fn write_resource(&self, type_name: &str, value: &rhai::Dynamic) {
        if !self.resource_bindings.contains_key(type_name) {
            log::warn!("write_resource: ресурс '{}' не зарегистрирован", type_name);
            return;
        }
        self.deferred_resource_writes.borrow_mut()
            .push((type_name.to_string(), value.clone()));
    }

    /// Отправить событие по имени типа (отложенно).
    ///
    /// Запрос буферизируется и применяется после завершения скрипта,
    /// чтобы избежать RefCell double-borrow при вызове внутри query()-итерации.
    pub fn emit_event(&self, type_name: &str, value: &rhai::Dynamic) {
        if !self.event_bindings.contains_key(type_name) {
            log::warn!("emit_event: событие '{}' не зарегистрировано", type_name);
            return;
        }
        self.deferred_events.borrow_mut()
            .push((type_name.to_string(), value.clone()));
    }
}

impl Default for ScriptContext {
    fn default() -> Self { Self::new() }
}