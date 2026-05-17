//! `ScriptContext` — контекст между Rust и Lua в пределах одного `run()`.
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
//! Lua-итератор удерживает shared borrow на World через ptr, поэтому
//! структурные изменения (spawn/despawn) нельзя применять внутри итерации.
//! Они накапливаются в `deferred: Commands` и применяются после завершения скрипта.

use std::{
    collections::HashMap,
    ptr::NonNull,
};

use apex_core::{
    commands::Commands,
    component::ComponentId,
    world::World,
};

use crate::registrar::{ResourceBinding, EventBinding};

// ── ComponentBinding ───────────────────────────────────────────

/// Информация о компоненте зарегистрированном для скриптинга.
///
/// Хранит функции конвертации компонент ↔ Lua без привязки к конкретному типу T.
#[derive(Clone)]
pub struct ComponentBinding {
    /// Имя типа компонента (совпадает с ключом в query-таблице)
    pub name: &'static str,
    /// ComponentId для поиска в архетипах
    pub id:   ComponentId,
    /// Читать компонент из Column[row] → mlua::Value
    pub read:  unsafe fn(*const u8, &mlua::Lua) -> mlua::Result<mlua::Value>,
    /// Записать компонент в Column[row] из mlua::Value; возвращает false если тип неверен
    pub write: unsafe fn(*mut u8, &mlua::Value) -> bool,
}

// ── SpawnRequest ───────────────────────────────────────────────

/// Запрос на создание entity, сформированный из скрипта.
///
/// Хранит список (name, RegistryKey) пар — компоненты для нового entity.
/// RegistryKey позволяет отложить извлечение mlua::Value до момента применения.
pub struct SpawnRequest {
    /// Список компонентов: (имя типа, RegistryKey с Lua-значением)
    pub components: Vec<(String, mlua::RegistryKey)>,
}

// ── ScriptContext ──────────────────────────────────────────────

/// Мост между Lua-скриптом и миром ECS.
///
/// Хранится в `Arc<RefCell<ScriptContext>>` в `ScriptEngine`.
/// Доступ через `lua.set_app_data()` / `lua.app_data_ref()`.
pub struct ScriptContext {
    /// Текущий delta time кадра — устанавливается перед `run()`
    pub delta_time: f32,

    /// Сырой указатель на мир. Живёт ровно в пределах `run()`.
    /// `None` означает что мы вне `run()` — любое обращение через скрипт
    /// вернёт ошибку вместо UB.
    world_ptr: Option<NonNull<World>>,

    /// Буфер отложенных команд spawn/despawn.
    /// Применяется после завершения скрипта через `apply_deferred()`.
    pub(crate) deferred: Commands,

    /// Буфер запросов spawn из скриптов.
    pub(crate) deferred_spawns: Vec<SpawnRequest>,

    /// Реестр компонентов доступных из скриптов: name → binding
    pub(crate) bindings: HashMap<&'static str, ComponentBinding>,

    /// Реестр ресурсов доступных из скриптов: name → binding
    pub(crate) resource_bindings: HashMap<&'static str, ResourceBinding>,

    /// Реестр событий доступных из скриптов: name → binding
    pub(crate) event_bindings: HashMap<&'static str, EventBinding>,

    /// Буфер отложенных записей ресурсов: (type_name, RegistryKey)
    /// Применяется после завершения скрипта.
    pub(crate) deferred_resource_writes: Vec<(&'static str, mlua::RegistryKey)>,

    /// Буфер отложенных событий: (type_name, RegistryKey)
    /// Применяется после завершения скрипта.
    pub(crate) deferred_events: Vec<(&'static str, mlua::RegistryKey)>,

    /// Счётчик entity — кешируется чтобы не вызывать world через ptr каждый раз
    entity_count_cache: usize,

    /// Кэш результатов сборки архетипов — избегает повторного сканирования
    /// при повторных query() с теми же дескрипторами.
    /// Инвалидируется при каждом новом запуске скрипта (в set_world_ptr).
    pub(crate) query_cache: HashMap<Vec<String>, Vec<crate::iterators::ArchState>>,

    /// Автоматически вызывать commit(entity) при переходе к следующей entity в query-итераторе
    pub auto_commit: bool,
}

impl ScriptContext {
    pub fn new() -> Self {
        Self {
            delta_time:              0.0,
            world_ptr:               None,
            deferred:                Commands::new(),
            deferred_spawns:         Vec::new(),
            bindings:                HashMap::new(),
            resource_bindings:       HashMap::new(),
            event_bindings:          HashMap::new(),
            deferred_resource_writes: Vec::new(),
            deferred_events:         Vec::new(),
            entity_count_cache:      0,
            query_cache:             HashMap::new(),
            auto_commit:             false,
        }
    }

    // ── Lifetime management ────────────────────────────────────

    /// Установить указатель на мир перед выполнением скрипта.
    pub(crate) unsafe fn set_world_ptr(&mut self, world: &mut World) {
        self.world_ptr         = Some(NonNull::new_unchecked(world as *mut World));
        self.entity_count_cache = world.entity_count();
        self.deferred.clear();
        self.deferred_resource_writes.clear();
        self.deferred_events.clear();
        self.query_cache.clear();
    }

    /// Сбросить указатель на мир после завершения скрипта.
    pub(crate) fn clear_world_ptr(&mut self) {
        self.world_ptr = None;
    }

    /// Получить `&World` — только для чтения (query-итераторы).
    pub(crate) fn world_ref(&self) -> &World {
        unsafe {
            self.world_ptr
                .expect("ScriptContext::world_ref вызван вне run()")
                .as_ref()
        }
    }

    /// Получить `&mut World` — для применения deferred команд.
    pub(crate) unsafe fn world_mut(&mut self) -> &mut World {
        self.world_ptr
            .expect("ScriptContext::world_mut вызван вне run()")
            .as_mut()
    }

    // ── API для Lua-функций ───────────────────────────────────

    pub fn delta_time(&self) -> f32 {
        self.delta_time
    }

    pub fn entity_count(&self) -> usize {
        self.entity_count_cache
    }

    pub fn queue_spawn(&mut self, request: SpawnRequest) {
        self.deferred_spawns.push(request);
    }

    pub fn queue_despawn(&mut self, entity: apex_core::Entity) {
        self.deferred.despawn(entity);
    }

    pub(crate) fn apply_deferred(&mut self) {
        let mut deferred = std::mem::take(&mut self.deferred);
        let world = unsafe { self.world_mut() };
        deferred.apply(world);
        self.deferred = deferred;
    }

    /// Применить отложенные записи ресурсов и отправки событий.
    pub(crate) fn apply_deferred_resources_and_events(
        &mut self,
        lua: &mlua::Lua,
    ) {
        let writes = std::mem::take(&mut self.deferred_resource_writes);
        let events = std::mem::take(&mut self.deferred_events);

        if writes.is_empty() && events.is_empty() {
            return;
        }

        // Собираем биндинги до заимствования world
        let write_infos: Vec<(&'static str, fn(&mlua::Value, &mut World) -> bool)> = writes.iter()
            .filter_map(|(name, _)| {
                self.resource_bindings.get(name)
                    .map(|b| (*name, b.write))
            })
            .collect();

        let emit_infos: Vec<(&'static str, fn(&mlua::Value, &mut World) -> bool)> = events.iter()
            .filter_map(|(name, _)| {
                self.event_bindings.get(name)
                    .map(|b| (*name, b.emit))
            })
            .collect();

        let world = unsafe { self.world_mut() };

        for (type_name, key) in writes {
            let val: mlua::Value = match lua.registry_value(&key) {
                Ok(v) => v,
                Err(_) => continue,
            };
            for (name, write_fn) in &write_infos {
                if *name == type_name {
                    write_fn(&val, world);
                }
            }
            let _ = lua.remove_registry_value(key);
        }

        for (type_name, key) in events {
            let val: mlua::Value = match lua.registry_value(&key) {
                Ok(v) => v,
                Err(_) => continue,
            };
            for (name, emit_fn) in &emit_infos {
                if *name == type_name {
                    emit_fn(&val, world);
                }
            }
            let _ = lua.remove_registry_value(key);
        }
    }

    // ── Регистрация ───────────────────────────────────────────

    pub(crate) fn add_binding(&mut self, binding: ComponentBinding) {
        self.bindings.insert(binding.name, binding);
    }

    pub(crate) fn binding(&self, name: &str) -> Option<&ComponentBinding> {
        self.bindings.get(name)
    }

    pub(crate) fn add_resource_binding(&mut self, binding: ResourceBinding) {
        self.resource_bindings.insert(binding.name, binding);
    }

    #[allow(dead_code)]
    pub(crate) fn resource_binding(&self, name: &str) -> Option<&ResourceBinding> {
        self.resource_bindings.get(name)
    }

    pub(crate) fn add_event_binding(&mut self, binding: EventBinding) {
        self.event_bindings.insert(binding.name, binding);
    }

    #[allow(dead_code)]
    pub(crate) fn event_binding(&self, name: &str) -> Option<&EventBinding> {
        self.event_bindings.get(name)
    }

    // ── Доступ к ресурсам из Lua ──────────────────────────────

    pub fn read_resource(
        &self,
        lua: &mlua::Lua,
        type_name: &str,
    ) -> Option<mlua::Value> {
        let binding = self.resource_bindings.get(type_name)?;
        let world = self.world_ref();
        (binding.read)(lua, world).ok()
    }

    pub fn write_resource(
        &mut self,
        lua: &mlua::Lua,
        type_name: &'static str,
        value: mlua::Value,
    ) -> mlua::Result<()> {
        if !self.resource_bindings.contains_key(type_name) {
            log::warn!("write_resource: ресурс '{}' не зарегистрирован", type_name);
            return Ok(());
        }
        let key = lua.create_registry_value(value)?;
        self.deferred_resource_writes.push((type_name, key));
        Ok(())
    }

    pub fn emit_event(
        &mut self,
        lua: &mlua::Lua,
        type_name: &'static str,
        value: mlua::Value,
    ) -> mlua::Result<()> {
        if !self.event_bindings.contains_key(type_name) {
            log::warn!("emit_event: событие '{}' не зарегистрировано", type_name);
            return Ok(());
        }
        let key = lua.create_registry_value(value)?;
        self.deferred_events.push((type_name, key));
        Ok(())
    }
}

impl Default for ScriptContext {
    fn default() -> Self { Self::new() }
}
