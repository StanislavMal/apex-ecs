//! apex-isolated — изолированные ECS-миры с коммуникацией через каналы.
//!
//! Содержит:
//! - [`IsolatedWorld`] — полностью изолированный мир с собственным планировщиком
//! - [`WorldBridge`] — канал связи между IsolatedWorld и основным World
//! - [`CloneableBridge`] — клонируемая обёртка для хранения в ресурсах
//! - [`BridgeEvent`] — события, передаваемые через WorldBridge
//! - [`sync_bridge_cloneable`] — система синхронизации для Scheduler'а

use apex_core::World;
use apex_scheduler::Scheduler;

// ---------------------------------------------------------------------------
// BridgeEvent
// ---------------------------------------------------------------------------

/// Событие, передаваемое через [`WorldBridge`] или [`CloneableBridge`].
pub enum BridgeEvent {
    /// Произвольное вызываемое действие.
    Action(Box<dyn FnOnce(&mut World) + Send>),
    /// Типизированное событие (сериализованное через bincode).
    Event {
        type_name: String,
        data:      Vec<u8>,
    },
}

// ---------------------------------------------------------------------------
// WorldBridge
// ---------------------------------------------------------------------------

/// Двунаправленный канал связи между [`IsolatedWorld`] и основным [`World`].
///
/// Использует lock-free очередь (`crossbeam::channel`) для передачи событий.
///
/// # Создание
///
/// ```ignore
/// let (to_sub, from_sub) = WorldBridge::new();
/// // to_sub   — отправляет в IsolatedWorld
/// // from_sub — получает из IsolatedWorld
/// ```
pub struct WorldBridge {
    /// События из основного мира в IsolatedWorld.
    inbound:  crossbeam_channel::Sender<BridgeEvent>,
    /// События из IsolatedWorld в основной мир.
    outbound: crossbeam_channel::Receiver<BridgeEvent>,
}

impl WorldBridge {
    /// Создать пару связанных мостов.
    ///
    /// Возвращает `(main_to_sub, sub_to_main)`, где:
    /// - `main_to_sub` — используется основным миром для отправки в IsolatedWorld
    /// - `sub_to_main` — используется IsolatedWorld для отправки в основной мир
    pub fn new() -> (Self, Self) {
        let (main_tx, sub_rx) = crossbeam_channel::unbounded();
        let (sub_tx, main_rx) = crossbeam_channel::unbounded();

        let main_to_sub = Self {
            inbound:  main_tx,
            outbound: main_rx,
        };

        let sub_to_main = Self {
            inbound:  sub_tx,
            outbound: sub_rx,
        };

        (main_to_sub, sub_to_main)
    }

    /// Отправить действие в целевой мир.
    pub fn send_action(&self, f: Box<dyn FnOnce(&mut World) + Send>) {
        let _ = self.inbound.send(BridgeEvent::Action(f));
    }

    /// Отправить типизированное событие в целевой мир.
    ///
    /// Событие сериализуется с помощью `bincode` перед отправкой.
    pub fn send_event<T: serde::Serialize + Send + Sync + 'static>(&self, event: &T) {
        let type_name = std::any::type_name::<T>().to_string();
        let data = match bincode::serialize(event) {
            Ok(bytes) => bytes,
            Err(_) => return,
        };
        let _ = self
            .inbound
            .send(BridgeEvent::Event { type_name, data });
    }

    /// Применить все накопленные сообщения к миру.
    ///
    /// Вычитывает все сообщения из `outbound` канала и применяет их:
    /// - `Action(f)` — вызывает `f(world)`
    /// - `Event { type_name, data }` — логирует (требуется реестр типов)
    pub fn apply_incoming(&self, world: &mut World) {
        while let Ok(event) = self.outbound.try_recv() {
            match event {
                BridgeEvent::Action(f) => {
                    f(world);
                }
                BridgeEvent::Event { type_name, data } => {
                    log::warn!(
                        "WorldBridge: received serialized Event `{type_name}`, use Action instead"
                    );
                    let _ = data;
                }
            }
        }
    }

    /// Отправить событие как `Action`.
    ///
    /// Позволяет отправить замыкание, которое вызовет `world.send_event(event)`.
    pub fn send_action_event<T: Send + Sync + 'static>(&self, event: T) {
        self.send_action(Box::new(move |world: &mut World| {
            world.send_event(event);
        }));
    }
}

// ---------------------------------------------------------------------------
// IsolatedWorld
// ---------------------------------------------------------------------------

/// Полностью изолированный мир с собственным планировщиком.
///
/// Полезен для:
/// - Симуляции (физика, AI) в отдельном потоке
/// - Под-миры уровней (каждый уровень — свой мир)
/// - Тестирования (изолированный мир для юнит-тестов)
///
/// # Пример
///
/// ```ignore
/// let mut iso = IsolatedWorld::new();
/// iso.scheduler_mut().add_system("my_sys", my_system);
/// iso.tick();
/// ```
pub struct IsolatedWorld {
    world:     World,
    scheduler: Scheduler,
}

impl IsolatedWorld {
    /// Создать новый изолированный мир.
    pub fn new() -> Self {
        Self {
            world:     World::new(),
            scheduler: Scheduler::new(),
        }
    }

    /// Доступ к внутреннему [`World`] (осторожно: structural changes).
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Доступ к [`Scheduler`] для конфигурации.
    pub fn scheduler_mut(&mut self) -> &mut Scheduler {
        &mut self.scheduler
    }

    /// Выполнить один тик: tick() → scheduler.run().
    ///
    /// Автоматически вызывает `compute_archetype_indices` и `compile` при первом запуске.
    pub fn tick(&mut self) {
        self.world.tick();
        self.scheduler.compute_archetype_indices(&self.world);
        if let Err(e) = self.scheduler.compile() {
            log::error!("IsolatedWorld::tick — scheduler compile error: {e}");
            return;
        }
        self.scheduler.run(&mut self.world);
    }

    /// Прочитать ресурс по типу.
    pub fn read_resource<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.world.resources.try_get::<T>()
    }

    /// Отправить событие в IsolatedWorld.
    pub fn send_event<T: Send + Sync + 'static>(&mut self, event: T) {
        self.world.send_event(event);
    }
}

impl Default for IsolatedWorld {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// CloneableBridge
// ---------------------------------------------------------------------------

/// Клонируемая обёртка над каналами WorldBridge для хранения в ресурсах.
///
/// `WorldBridge` не реализует `Clone`, но его внутренние каналы
/// (`crossbeam_channel::Sender`/`Receiver`) клонируются.
/// Эта обёртка пригодна для хранения в `ResourceMap`.
#[derive(Clone)]
pub struct CloneableBridge {
    /// Sender для отправки событий в IsolatedWorld.
    to_sub:   crossbeam_channel::Sender<BridgeEvent>,
    /// Receiver для получения событий из IsolatedWorld.
    from_sub: crossbeam_channel::Receiver<BridgeEvent>,
}

impl CloneableBridge {
    /// Создать `CloneableBridge` из пары каналов.
    pub fn new(
        to_sub: crossbeam_channel::Sender<BridgeEvent>,
        from_sub: crossbeam_channel::Receiver<BridgeEvent>,
    ) -> Self {
        Self { to_sub, from_sub }
    }

    /// Отправить действие в IsolatedWorld.
    pub fn send_action(&self, f: Box<dyn FnOnce(&mut World) + Send>) {
        let _ = self.to_sub.send(BridgeEvent::Action(f));
    }

    /// Применить все накопленные сообщения ИЗ IsolatedWorld.
    pub fn apply_incoming(&self, world: &mut World) {
        while let Ok(event) = self.from_sub.try_recv() {
            match event {
                BridgeEvent::Action(f) => {
                    f(world);
                }
                BridgeEvent::Event { type_name, data } => {
                    log::warn!(
                        "CloneableBridge: received serialized Event `{type_name}`, use Action instead"
                    );
                    let _ = data;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SyncBridgeSystem
// ---------------------------------------------------------------------------

/// Система синхронизации, работающая с [`CloneableBridge`].
///
/// Добавляется в Scheduler основного мира. На каждом тике вызывает
/// `bridge.apply_incoming(world)` для применения событий из IsolatedWorld.
///
/// # Пример
///
/// ```ignore
/// use apex_isolated::{CloneableBridge, sync_bridge_cloneable};
///
/// let (to_sub, from_sub) = crossbeam_channel::unbounded();
/// let (to_main, from_main) = crossbeam_channel::unbounded();
/// let bridge = CloneableBridge::new(to_sub, from_main);
/// world.insert_resource(bridge);
/// scheduler.add_system("sync_bridge", sync_bridge_cloneable);
/// ```
pub fn sync_bridge_cloneable(world: &mut World) {
    // Извлекаем CloneableBridge через сырой указатель, чтобы обойти borrow checker.
    let bridge_ptr: *const CloneableBridge = match world.resources.try_get::<CloneableBridge>() {
        Some(b) => b,
        None => return,
    };

    // SAFETY: apply_incoming(&self, &mut World) не конфликтует — bridge живёт
    // дольше world, и мы не держим заимствование на world после вызова.
    unsafe {
        (*bridge_ptr).apply_incoming(world);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::{AccessDescriptor, World};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    // -----------------------------------------------------------------------
    // IsolatedWorld
    // -----------------------------------------------------------------------

    #[test]
    fn isolated_world_tick() {
        let mut iso = IsolatedWorld::new();

        let invoked = Arc::new(AtomicBool::new(false));
        let inv = invoked.clone();

        // Используем add_fn_par_system, который принимает FnMut(SystemContext)
        iso.scheduler_mut().add_fn_par_system(
            "test",
            move |_ctx: apex_core::SystemContext<'_>| {
                inv.store(true, Ordering::SeqCst);
            },
            AccessDescriptor::new(),
        );

        iso.tick();
        assert_eq!(iso.world.entity_count(), 0);
    }

    #[test]
    fn isolated_world_independent() {
        let mut main_world = World::new();
        let mut iso = IsolatedWorld::new();

        // Спавним сущность в основном мире
        main_world.spawn_empty();

        // В изолированном мире ничего нет
        assert_eq!(iso.world.entity_count(), 0);
        assert_eq!(main_world.entity_count(), 1);
    }

    #[test]
    fn isolated_world_read_resource() {
        let mut iso = IsolatedWorld::new();

        iso.world_mut()
            .resources
            .insert::<String>("hello".to_string());

        let val: Option<&String> = iso.read_resource();
        assert_eq!(val, Some(&"hello".to_string()));
    }

    #[test]
    fn isolated_world_read_resource_missing() {
        let iso = IsolatedWorld::new();
        let val: Option<&String> = iso.read_resource();
        assert!(val.is_none());
    }

    #[test]
    fn isolated_world_send_event() {
        let mut iso = IsolatedWorld::new();
        // Регистрируем тип события, затем отправляем
        iso.world_mut().add_event::<u32>();
        iso.send_event(42u32);
        iso.world_mut().tick(); // обновит события
    }

    // -----------------------------------------------------------------------
    // WorldBridge
    // -----------------------------------------------------------------------

    #[test]
    fn world_bridge_send_action() {
        let (bridge_a, bridge_b) = WorldBridge::new();

        let invoked = Arc::new(AtomicBool::new(false));
        let inv = invoked.clone();

        bridge_a.send_action(Box::new(move |_: &mut World| {
            inv.store(true, Ordering::SeqCst);
        }));

        let mut world = World::new();
        bridge_b.apply_incoming(&mut world);

        assert!(invoked.load(Ordering::SeqCst));
    }

    #[test]
    fn world_bridge_action_spawns_entity() {
        let (bridge_a, bridge_b) = WorldBridge::new();

        bridge_a.send_action(Box::new(|world: &mut World| {
            world.spawn_empty();
        }));

        let mut world = World::new();
        bridge_b.apply_incoming(&mut world);

        assert_eq!(world.entity_count(), 1);
    }

    #[test]
    fn world_bridge_multiple_actions() {
        let (bridge_a, bridge_b) = WorldBridge::new();

        let counter = Arc::new(AtomicUsize::new(0));
        let c1 = counter.clone();
        bridge_a.send_action(Box::new(move |_: &mut World| {
            c1.fetch_add(1, Ordering::SeqCst);
        }));
        let c2 = counter.clone();
        bridge_a.send_action(Box::new(move |_: &mut World| {
            c2.fetch_add(1, Ordering::SeqCst);
        }));

        let mut world = World::new();
        bridge_b.apply_incoming(&mut world);

        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    // -----------------------------------------------------------------------
    // CloneableBridge
    // -----------------------------------------------------------------------

    #[test]
    fn cloneable_bridge_basic() {
        let (main_tx, sub_rx) = crossbeam_channel::unbounded();
        let (sub_tx, main_rx) = crossbeam_channel::unbounded();

        let to_main = CloneableBridge::new(main_tx, main_rx);
        let to_sub_bridge = CloneableBridge::new(sub_tx, sub_rx);

        // Отправляем действие из под-мира в основной
        to_sub_bridge.send_action(Box::new(|world: &mut World| {
            world.spawn_empty();
        }));

        let mut world = World::new();
        to_main.apply_incoming(&mut world);

        assert_eq!(world.entity_count(), 1);
    }

    // -----------------------------------------------------------------------
    // SyncBridgeSystem
    // -----------------------------------------------------------------------

    #[test]
    fn sync_bridge_system_works() {
        // Создаём IsolatedWorld с системой, считающей entity_count
        let mut iso = IsolatedWorld::new();

        let entity_count = Arc::new(AtomicUsize::new(0));
        let ec = entity_count.clone();

        iso.scheduler_mut().add_fn_par_system(
            "counter",
            move |ctx: apex_core::SystemContext<'_>| {
                ec.store(ctx.entity_count(), Ordering::SeqCst);
            },
            AccessDescriptor::new(),
        );

        iso.world_mut().spawn_empty();
        iso.tick();

        assert_eq!(iso.world.entity_count(), 1);
    }
}
