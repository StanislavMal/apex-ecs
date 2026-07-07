//! apex-isolated — isolated ECS worlds communicating over channels.
//!
//! Contains:
//! - [`IsolatedWorld`] — a fully isolated world with its own scheduler
//! - [`WorldBridge`] — a communication channel between an IsolatedWorld and the main World
//! - [`CloneableBridge`] — a cloneable wrapper for storing in resources
//! - [`BridgeEvent`] — events passed through a WorldBridge
//! - [`sync_bridge_cloneable`] — a synchronization system for the Scheduler
//! - [`exchange`] — cross-world entity transfer with reference remapping (В4)
//! - [`WorldRegistrar`] — a replayable world-schema recipe (shared registration)

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use apex_core::World;
use apex_scheduler::Scheduler;

pub mod exchange;
pub mod registrar;

pub use registrar::WorldRegistrar;

// ---------------------------------------------------------------------------
// BridgeEvent
// ---------------------------------------------------------------------------

/// An event passed through a [`WorldBridge`] or [`CloneableBridge`].
pub enum BridgeEvent {
    /// An arbitrary callable action.
    Action(Box<dyn FnOnce(&mut World) + Send>),
    /// A typed event (serialized via bincode).
    Event { type_name: String, data: Vec<u8> },
}

// ---------------------------------------------------------------------------
// Bounded channels — backpressure policy + telemetry (В4)
// ---------------------------------------------------------------------------

/// What a bounded bridge does when its queue is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeOverflow {
    /// Block the sender until the receiver drains space — lossless backpressure.
    /// Correct when both worlds are ticking; a stalled receiver stalls the sender
    /// (visible), never silently unbounded memory growth.
    Block,
    /// Drop the message, bump the dropped counter, and warn once — lossy but
    /// non-blocking. For soft-real-time producers that must not stall (a render
    /// or telemetry feed) and prefer to shed load loudly.
    DropLoud,
}

/// Configuration for a bridge's channel.
///
/// The pre-В4 bridge used **unbounded** channels: a slow consumer grew memory
/// without limit and without any signal. A bounded channel + explicit overflow
/// policy makes backpressure a decision, not an accident.
#[derive(Clone, Copy, Debug)]
pub struct BridgeConfig {
    /// Channel capacity (messages) per direction.
    pub capacity: usize,
    /// Behaviour when the channel is full.
    pub overflow: BridgeOverflow,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        // Lossless by default: bounded so memory is capped, blocking so nothing
        // is silently lost. A soft-real-time producer opts into `DropLoud`.
        Self { capacity: 1024, overflow: BridgeOverflow::Block }
    }
}

/// Live counters for one direction of a bridge. Shared (`Arc`) with the peer's
/// receiver so both ends can observe the same figures.
#[derive(Debug, Default)]
pub struct BridgeStats {
    /// Messages successfully enqueued.
    pub sent:       AtomicU64,
    /// Messages dropped (channel full under `DropLoud`, or peer disconnected).
    pub dropped:    AtomicU64,
    /// Peak observed queue depth — how close the channel came to saturation.
    pub high_water: AtomicU64,
}

impl BridgeStats {
    fn record_sent(&self, queue_len: usize) {
        self.sent.fetch_add(1, Ordering::Relaxed);
        // Monotonic max — best-effort under Relaxed (telemetry, not a gate).
        let len = queue_len as u64;
        let mut hw = self.high_water.load(Ordering::Relaxed);
        while len > hw {
            match self.high_water.compare_exchange_weak(
                hw, len, Ordering::Relaxed, Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(cur) => hw = cur,
            }
        }
    }

    fn record_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Messages successfully enqueued so far.
    pub fn sent(&self) -> u64 { self.sent.load(Ordering::Relaxed) }
    /// Messages dropped so far (full channel under `DropLoud`, or disconnect).
    pub fn dropped(&self) -> u64 { self.dropped.load(Ordering::Relaxed) }
    /// Peak observed queue depth.
    pub fn high_water(&self) -> u64 { self.high_water.load(Ordering::Relaxed) }
}

/// Enqueue `ev` on `tx` honouring `config`, recording telemetry. Returns whether
/// the message was accepted. A closed peer or a full `DropLoud` channel are both
/// surfaced loudly (§0.2a) and counted, never swallowed silently.
fn send_with_policy(
    tx: &crossbeam_channel::Sender<BridgeEvent>,
    ev: BridgeEvent,
    config: BridgeConfig,
    stats: &BridgeStats,
    what: &str,
) -> bool {
    use crossbeam_channel::TrySendError;
    match config.overflow {
        BridgeOverflow::Block => match tx.send(ev) {
            Ok(()) => {
                stats.record_sent(tx.len());
                true
            }
            Err(_) => {
                stats.record_dropped();
                apex_core::warn_once!("bridge send ({what}): peer channel closed — message dropped");
                false
            }
        },
        BridgeOverflow::DropLoud => match tx.try_send(ev) {
            Ok(()) => {
                stats.record_sent(tx.len());
                true
            }
            Err(TrySendError::Full(_)) => {
                stats.record_dropped();
                apex_core::warn_once!(
                    "bridge send ({what}): channel full (capacity reached) — message dropped (DropLoud)"
                );
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                stats.record_dropped();
                apex_core::warn_once!("bridge send ({what}): peer channel closed — message dropped");
                false
            }
        },
    }
}

// ---------------------------------------------------------------------------
// WorldBridge
// ---------------------------------------------------------------------------

/// A bidirectional communication channel between an [`IsolatedWorld`] and the main [`World`].
///
/// Uses a lock-free queue (`crossbeam::channel`) to pass events.
///
/// # Creation
///
/// ```ignore
/// let (to_sub, from_sub) = WorldBridge::new();
/// // to_sub   — sends into the IsolatedWorld
/// // from_sub — receives from the IsolatedWorld
/// ```
type EventHandler = Box<dyn Fn(&[u8], &mut World) + Send + Sync>;

pub struct WorldBridge {
    /// Events from the main world into the IsolatedWorld.
    inbound: crossbeam_channel::Sender<BridgeEvent>,
    /// Events from the IsolatedWorld into the main world.
    outbound: crossbeam_channel::Receiver<BridgeEvent>,
    /// Registry of deserializers: type_name → (data, world).
    event_handlers: Arc<RwLock<HashMap<String, EventHandler>>>,
    /// Channel capacity + overflow policy for this direction's sends.
    config: BridgeConfig,
    /// Telemetry for this direction's sends.
    stats: Arc<BridgeStats>,
}

impl WorldBridge {
    /// Create a pair of linked bridges with the default (bounded, blocking) config.
    ///
    /// Returns `(main_to_sub, sub_to_main)`, where:
    /// - `main_to_sub` — used by the main world to send into the IsolatedWorld
    /// - `sub_to_main` — used by the IsolatedWorld to send into the main world
    pub fn new() -> (Self, Self) {
        Self::with_config(BridgeConfig::default())
    }

    /// Create a pair of linked bridges with an explicit channel config (В4).
    pub fn with_config(config: BridgeConfig) -> (Self, Self) {
        let (main_tx, sub_rx) = crossbeam_channel::bounded(config.capacity);
        let (sub_tx, main_rx) = crossbeam_channel::bounded(config.capacity);

        let handlers = Arc::new(RwLock::new(HashMap::new()));

        let main_to_sub = Self {
            inbound: main_tx,
            outbound: main_rx,
            event_handlers: Arc::clone(&handlers),
            config,
            stats: Arc::new(BridgeStats::default()),
        };

        let sub_to_main = Self {
            inbound: sub_tx,
            outbound: sub_rx,
            event_handlers: handlers,
            config,
            stats: Arc::new(BridgeStats::default()),
        };

        (main_to_sub, sub_to_main)
    }

    /// Telemetry for this direction's sends (sent / dropped / peak depth).
    pub fn stats(&self) -> &BridgeStats {
        &self.stats
    }

    /// Register an event type for deserialization on the receiving side.
    ///
    /// Calls `world.add_event::<T>()` and stores the deserializer
    /// in the bridge's shared registry.
    pub fn register_event<T>(&self, world: &mut World)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        world.add_event::<T>();
        let type_name = std::any::type_name::<T>().to_string();
        self.event_handlers.write().unwrap().insert(
            type_name,
            Box::new(|data: &[u8], world: &mut World| {
                if let Ok(event) = bincode::deserialize::<T>(data) {
                    world.send_event(event);
                } else {
                    log::warn!(
                        "WorldBridge: failed to deserialize Event `{}`",
                        std::any::type_name::<T>()
                    );
                }
            }),
        );
    }

    /// Send an action to the target world.
    ///
    /// Honours the bridge's overflow policy; a closed peer or a full `DropLoud`
    /// channel is surfaced loudly and counted in [`stats`](Self::stats) (§0.2a).
    pub fn send_action(&self, f: Box<dyn FnOnce(&mut World) + Send>) {
        send_with_policy(
            &self.inbound,
            BridgeEvent::Action(f),
            self.config,
            &self.stats,
            "WorldBridge::send_action",
        );
    }

    /// Send a typed event to the target world.
    ///
    /// The event is serialized with `bincode` before sending.
    /// [`register_event::<T>`](Self::register_event) must be called on the receiving side.
    pub fn send_event<T: serde::Serialize + Send + Sync + 'static>(&self, event: &T) {
        let type_name = std::any::type_name::<T>().to_string();
        let data = match bincode::serialize(event) {
            Ok(bytes) => bytes,
            // §0.2a (E12): a serialize failure silently discarded the event.
            Err(e) => {
                self.stats.record_dropped();
                apex_core::warn_once!(
                    "WorldBridge::send_event: bincode serialize of `{type_name}` failed ({e}) — event dropped"
                );
                return;
            }
        };
        send_with_policy(
            &self.inbound,
            BridgeEvent::Event { type_name, data },
            self.config,
            &self.stats,
            "WorldBridge::send_event",
        );
    }

    /// Apply all accumulated messages to the world.
    ///
    /// Drains all messages from the `outbound` channel and applies them:
    /// - `Action(f)` — calls `f(world)`
    /// - `Event { type_name, data }` — deserializes via the registered
    ///   handler and calls `world.send_event()`
    pub fn apply_incoming(&self, world: &mut World) {
        while let Ok(event) = self.outbound.try_recv() {
            match event {
                BridgeEvent::Action(f) => {
                    f(world);
                }
                BridgeEvent::Event { type_name, data } => {
                    let handlers = self.event_handlers.read().unwrap();
                    if let Some(handler) = handlers.get(&type_name) {
                        handler(&data, world);
                    } else {
                        log::warn!(
                            "WorldBridge: received serialized Event `{type_name}` — \
                             no handler registered. Call bridge.register_event::<{type_name}>() first."
                        );
                    }
                }
            }
        }
    }

    /// Send an event as an `Action`.
    ///
    /// Lets you send a closure that will call `world.send_event(event)`.
    /// Unlike [`send_event`](Self::send_event), it requires no serialization and
    /// works without a registry on the receiving side.
    pub fn send_action_event<T: Send + Sync + 'static>(&self, event: T) {
        self.send_action(Box::new(move |world: &mut World| {
            world.send_event(event);
        }));
    }
}

// ---------------------------------------------------------------------------
// IsolatedWorld
// ---------------------------------------------------------------------------

/// A fully isolated world with its own scheduler.
///
/// Useful for:
/// - Simulation (physics, AI) on a separate thread
/// - Level sub-worlds (each level is its own world)
/// - Testing (an isolated world for unit tests)
///
/// # Example
///
/// ```ignore
/// let mut iso = IsolatedWorld::new();
/// iso.scheduler_mut().add_system("my_sys", my_system);
/// iso.tick();
/// ```
pub struct IsolatedWorld {
    world: World,
    scheduler: Scheduler,
}

// SAFETY (Send): an `IsolatedWorld` is *owned* by whichever thread runs it — the
// pipelined renderer moves it into its render thread (`spawn(move || …)`) and no
// other thread references it. Moving it across the thread boundary is sound as
// long as its contents are `Send`; the render/simulation worlds it holds contain
// only `Send` data (no `NonSend` resources), which is the standing contract for a
// world that ticks off the main thread. The auto-derived `Send` does not apply
// because a `World`/`Scheduler` may hold trait objects the compiler cannot prove
// `Send`, so this asserts the invariant explicitly.
//
// `Sync` is deliberately NOT implemented: nothing shares an `&IsolatedWorld`
// across threads (the old `unsafe impl Sync` had a false justification — E12 —
// and no consumer required it; the renderer transfers ownership, it does not
// alias). Withholding `Sync` keeps the type honest.
unsafe impl Send for IsolatedWorld {}

impl IsolatedWorld {
    /// Create a new isolated world.
    pub fn new() -> Self {
        Self {
            world: World::new(),
            scheduler: Scheduler::new(),
        }
    }

    /// Create an isolated world whose schema is registered from a shared recipe
    /// (В4 / B5) — the same components/events/relations as the world it will
    /// exchange data with, without hand-duplicating registrations.
    pub fn from_registrar(recipe: &WorldRegistrar) -> Self {
        Self {
            world: recipe.new_world(),
            scheduler: Scheduler::new(),
        }
    }

    /// Export this world's entire state as a snapshot (fork it out).
    /// See [`exchange`] for the subtree-scoped and apply-back variants.
    pub fn export(
        &self,
        ctx: &mut dyn apex_core::SerdeContext,
    ) -> Result<apex_serialization::WorldSnapshot, apex_serialization::SerializationError> {
        exchange::export_world(&self.world, ctx)
    }

    /// Import a snapshot into this world (fresh entities, `Entity` refs remapped);
    /// returns the old-index → new-`Entity` map.
    pub fn import(
        &mut self,
        snapshot: &apex_serialization::WorldSnapshot,
        ctx:      &mut dyn apex_core::SerdeContext,
    ) -> Result<apex_serialization::RestoreEntityMap, apex_serialization::SerializationError> {
        exchange::import(&mut self.world, snapshot, ctx)
    }

    /// Access to the internal [`World`] (careful: structural changes).
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Swap the internal World with another.
    /// Used by pipelined rendering to exchange worlds between threads.
    pub fn swap_world(&mut self, other: &mut World) {
        std::mem::swap(&mut self.world, other);
    }

    /// Access to the [`Scheduler`] for configuration.
    pub fn scheduler_mut(&mut self) -> &mut Scheduler {
        &mut self.scheduler
    }

    /// Perform a single tick: tick() → scheduler.run().
    ///
    /// `compile` is called explicitly to log the error gracefully (otherwise `run`
    /// panics); the systems→archetypes mapping is recomputed by `run` itself on each call.
    pub fn tick(&mut self) {
        self.world.tick();
        if let Err(e) = self.scheduler.compile() {
            log::error!("IsolatedWorld::tick — scheduler compile error: {e}");
            return;
        }
        self.scheduler.run(&mut self.world);
    }

    /// Read a resource by type.
    pub fn read_resource<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.world.try_resource::<T>()
    }

    /// Send an event into the IsolatedWorld.
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

/// A cloneable wrapper over WorldBridge channels for storing in resources.
///
/// `WorldBridge` does not implement `Clone`, but its internal channels
/// (`crossbeam_channel::Sender`/`Receiver`) are cloneable.
/// This wrapper is suitable for storing in `Resources`.
#[derive(Clone)]
pub struct CloneableBridge {
    /// Sender for sending events into the IsolatedWorld.
    to_sub: crossbeam_channel::Sender<BridgeEvent>,
    /// Receiver for receiving events from the IsolatedWorld.
    from_sub: crossbeam_channel::Receiver<BridgeEvent>,
    /// Registry of deserializers.
    event_handlers: Arc<RwLock<HashMap<String, EventHandler>>>,
    /// Overflow policy applied on send (the channels are caller-provided, so
    /// their capacity is whatever the caller chose; only the policy lives here).
    config: BridgeConfig,
    /// Telemetry for this bridge's sends (shared across clones — same channels).
    stats: Arc<BridgeStats>,
}

impl CloneableBridge {
    /// Create a `CloneableBridge` from a pair of channels (default blocking policy).
    ///
    /// The caller supplies the channels, so bound them (`crossbeam_channel::bounded`)
    /// for backpressure; an unbounded pair keeps the pre-В4 grow-without-limit
    /// behaviour, now an explicit caller choice rather than the only option.
    pub fn new(
        to_sub: crossbeam_channel::Sender<BridgeEvent>,
        from_sub: crossbeam_channel::Receiver<BridgeEvent>,
    ) -> Self {
        Self::with_overflow(to_sub, from_sub, BridgeConfig::default().overflow)
    }

    /// Create a `CloneableBridge` with an explicit overflow policy (В4).
    pub fn with_overflow(
        to_sub: crossbeam_channel::Sender<BridgeEvent>,
        from_sub: crossbeam_channel::Receiver<BridgeEvent>,
        overflow: BridgeOverflow,
    ) -> Self {
        Self {
            to_sub,
            from_sub,
            event_handlers: Arc::new(RwLock::new(HashMap::new())),
            config: BridgeConfig { capacity: 0, overflow },
            stats: Arc::new(BridgeStats::default()),
        }
    }

    /// Telemetry for this bridge's sends (sent / dropped / peak depth).
    pub fn stats(&self) -> &BridgeStats {
        &self.stats
    }

    /// Register an event type for deserialization on the receiving side.
    ///
    /// Calls `world.add_event::<T>()` and stores the deserializer.
    pub fn register_event<T>(&self, world: &mut World)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        world.add_event::<T>();
        let type_name = std::any::type_name::<T>().to_string();
        self.event_handlers.write().unwrap().insert(
            type_name,
            Box::new(|data: &[u8], world: &mut World| {
                if let Ok(event) = bincode::deserialize::<T>(data) {
                    world.send_event(event);
                } else {
                    log::warn!(
                        "CloneableBridge: failed to deserialize Event `{}`",
                        std::any::type_name::<T>()
                    );
                }
            }),
        );
    }

    /// Send an action into the IsolatedWorld.
    ///
    /// Honours the overflow policy; a closed peer or a full `DropLoud` channel is
    /// surfaced loudly and counted in [`stats`](Self::stats) (§0.2a).
    pub fn send_action(&self, f: Box<dyn FnOnce(&mut World) + Send>) {
        send_with_policy(
            &self.to_sub,
            BridgeEvent::Action(f),
            self.config,
            &self.stats,
            "CloneableBridge::send_action",
        );
    }

    /// Send a typed event to the target world (serialized via bincode).
    ///
    /// [`register_event::<T>`](Self::register_event) must be called on the receiving side.
    pub fn send_event<T: serde::Serialize + Send + Sync + 'static>(&self, event: &T) {
        let type_name = std::any::type_name::<T>().to_string();
        let data = match bincode::serialize(event) {
            Ok(bytes) => bytes,
            // §0.2a (E12): a serialize failure silently discarded the event.
            Err(e) => {
                self.stats.record_dropped();
                apex_core::warn_once!(
                    "CloneableBridge::send_event: bincode serialize of `{type_name}` failed ({e}) — event dropped"
                );
                return;
            }
        };
        send_with_policy(
            &self.to_sub,
            BridgeEvent::Event { type_name, data },
            self.config,
            &self.stats,
            "CloneableBridge::send_event",
        );
    }

    /// Send an event as an `Action`.
    ///
    /// Unlike [`send_event`](Self::send_event), it requires no serialization
    /// and no registration on the receiving side.
    pub fn send_action_event<T: Send + Sync + 'static>(&self, event: T) {
        self.send_action(Box::new(move |world: &mut World| {
            world.send_event(event);
        }));
    }

    /// Apply all accumulated messages FROM the IsolatedWorld.
    pub fn apply_incoming(&self, world: &mut World) {
        while let Ok(event) = self.from_sub.try_recv() {
            match event {
                BridgeEvent::Action(f) => {
                    f(world);
                }
                BridgeEvent::Event { type_name, data } => {
                    let handlers = self.event_handlers.read().unwrap();
                    if let Some(handler) = handlers.get(&type_name) {
                        handler(&data, world);
                    } else {
                        log::warn!(
                            "CloneableBridge: received serialized Event `{type_name}` — \
                             no handler registered."
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SyncBridgeSystem
// ---------------------------------------------------------------------------

/// A synchronization system that works with a [`CloneableBridge`].
///
/// Added to the main world's Scheduler. On every tick it calls
/// `bridge.apply_incoming(world)` to apply events from the IsolatedWorld.
///
/// # Example
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
    // Clone the bridge (its channels/registry are Arc-shared, so the clone drives
    // the SAME queues) and release the resource borrow BEFORE draining. An incoming
    // Action may remove/replace the CloneableBridge resource — a borrow held across
    // apply_incoming would then dangle (UAF, E2). Cloning does this safely.
    let bridge = match world.try_resource::<CloneableBridge>() {
        Some(b) => b.clone(),
        None => return,
    };
    bridge.apply_incoming(world);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::World;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    // -----------------------------------------------------------------------
    // IsolatedWorld
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // §0.2a (E12): bridge send paths surface dropped events/actions instead of
    // swallowing them silently. Behaviour is a graceful no-op (no panic); the
    // loudness itself (warn_once) is covered by the apex-core macro test.
    // -----------------------------------------------------------------------

    #[test]
    fn e12_closed_channel_drops_without_panic() {
        let (main, sub) = WorldBridge::new();
        drop(sub); // receiver for `main.inbound` is gone → send() errors
        main.send_action(Box::new(|_w: &mut World| {}));
        main.send_event(&7u32);
        // The drop is counted, not swallowed silently (В4 telemetry).
        assert_eq!(main.stats().dropped(), 2);
        assert_eq!(main.stats().sent(), 0);
    }

    // -----------------------------------------------------------------------
    // В4: bounded channels + backpressure policy + telemetry
    // -----------------------------------------------------------------------

    #[test]
    fn iso_world_is_send_not_sync() {
        // Send is asserted (renderer moves the world into its thread); Sync is
        // deliberately withheld. This pins the Send contract at compile time.
        fn assert_send<T: Send>() {}
        assert_send::<IsolatedWorld>();
    }

    #[test]
    fn bridge_stats_count_successful_sends() {
        let (main, _sub) = WorldBridge::new();
        main.send_action(Box::new(|_w: &mut World| {}));
        main.send_action(Box::new(|_w: &mut World| {}));
        assert_eq!(main.stats().sent(), 2);
        assert_eq!(main.stats().dropped(), 0);
        assert!(main.stats().high_water() >= 1, "peak depth observed");
    }

    #[test]
    fn drop_loud_sheds_load_when_full_and_counts_it() {
        // A tiny DropLoud channel: fill it, then the next send is dropped (loud)
        // rather than blocking or growing memory. The consumer never drains here.
        let (to_sub, sub_rx) = crossbeam_channel::bounded(2);
        let (_unused_tx, from_sub) = crossbeam_channel::bounded::<BridgeEvent>(2);
        let bridge = CloneableBridge::with_overflow(to_sub, from_sub, BridgeOverflow::DropLoud);

        bridge.send_action(Box::new(|_w: &mut World| {})); // 1 → queued
        bridge.send_action(Box::new(|_w: &mut World| {})); // 2 → queued (full)
        bridge.send_action(Box::new(|_w: &mut World| {})); // 3 → dropped (loud)
        bridge.send_action(Box::new(|_w: &mut World| {})); // 4 → dropped (loud)

        assert_eq!(bridge.stats().sent(), 2, "capacity accepted");
        assert_eq!(bridge.stats().dropped(), 2, "overflow shed loudly");
        assert_eq!(sub_rx.len(), 2, "channel bounded at capacity");
    }

    #[test]
    fn block_policy_backpressures_without_dropping() {
        // With Block policy and a draining consumer on another thread, every
        // message is delivered — bounded memory, zero loss.
        let (to_sub, sub_rx) = crossbeam_channel::bounded(4);
        let (_unused_tx, from_sub) = crossbeam_channel::bounded::<BridgeEvent>(4);
        let bridge = CloneableBridge::with_overflow(to_sub, from_sub, BridgeOverflow::Block);

        let drainer = std::thread::spawn(move || {
            let mut count = 0;
            while let Ok(_ev) = sub_rx.recv() {
                count += 1;
                if count == 100 {
                    break;
                }
            }
            count
        });

        for _ in 0..100 {
            bridge.send_action(Box::new(|_w: &mut World| {}));
        }
        let received = drainer.join().unwrap();
        assert_eq!(received, 100, "Block policy loses nothing");
        assert_eq!(bridge.stats().sent(), 100);
        assert_eq!(bridge.stats().dropped(), 0);
    }

    #[test]
    fn e12_serialize_failure_drops_event_without_panic() {
        struct FailSer;
        impl serde::Serialize for FailSer {
            fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("intentional serialize failure"))
            }
        }
        let (main, _sub) = WorldBridge::new();
        // bincode::serialize fails before the channel is touched — event dropped.
        main.send_event(&FailSer);
    }

    #[test]
    fn isolated_world_tick() {
        let mut iso = IsolatedWorld::new();

        let invoked = Arc::new(AtomicBool::new(false));
        let inv = invoked.clone();

        iso.scheduler_mut().add_systems(
            apex_scheduler::StageLabel::Update,
            apex_scheduler::par("test", move |_ctx| {
                inv.store(true, Ordering::SeqCst);
            }),
        );

        iso.tick();
        assert_eq!(iso.world.entity_count(), 0);
    }

    #[test]
    fn isolated_world_independent() {
        let mut main_world = World::new();
        let iso = IsolatedWorld::new();

        // Spawn an entity in the main world
        main_world.spawn(());

        // The isolated world contains nothing
        assert_eq!(iso.world.entity_count(), 0);
        assert_eq!(main_world.entity_count(), 1);
    }

    #[test]
    fn isolated_world_read_resource() {
        let mut iso = IsolatedWorld::new();

        iso.world_mut()
            .insert_resource::<String>("hello".to_string());

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
        // Register the event type, then send
        iso.world_mut().add_event::<u32>();
        iso.send_event(42u32);
        iso.world_mut().tick(); // increments the tick
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
            world.spawn(());
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

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct ScoreEvent(u32);

    #[test]
    fn world_bridge_send_event_round_trip() {
        let (bridge_a, bridge_b) = WorldBridge::new();
        let mut world = World::new();

        bridge_b.register_event::<ScoreEvent>(&mut world);
        bridge_a.send_event(&ScoreEvent(42));
        bridge_b.apply_incoming(&mut world);

        assert_eq!(
            world.events::<ScoreEvent>().len(),
            1,
            "event should be in pending buffer"
        );
    }

    #[test]
    fn world_bridge_send_event_missing_handler() {
        let (bridge_a, bridge_b) = WorldBridge::new();
        let mut world = World::new();

        bridge_a.send_event(&ScoreEvent(99));
        // No crash, just a log warning
        bridge_b.apply_incoming(&mut world);
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

        // Send an action from the sub-world into the main one
        to_sub_bridge.send_action(Box::new(|world: &mut World| {
            world.spawn(());
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
        // Create an IsolatedWorld with a system that counts entity_count
        let mut iso = IsolatedWorld::new();

        let entity_count = Arc::new(AtomicUsize::new(0));
        let ec = entity_count.clone();

        iso.scheduler_mut().add_systems(
            apex_scheduler::StageLabel::Update,
            apex_scheduler::par("counter", move |ctx| {
                ec.store(ctx.entity_count(), Ordering::SeqCst);
            }),
        );

        iso.world_mut().spawn(());
        iso.tick();

        assert_eq!(iso.world.entity_count(), 1);
    }

    /// E2: an incoming Action that removes the CloneableBridge resource must not
    /// leave `sync_bridge_cloneable` dereferencing freed memory. The system now
    /// clones the bridge (Arc-shared channels) before draining, so the removal is
    /// safe.
    #[test]
    fn sync_bridge_action_removing_bridge_resource_does_not_uaf() {
        let (to_sub_tx, _to_sub_rx) = crossbeam_channel::unbounded();
        let (from_sub_tx, from_sub_rx) = crossbeam_channel::unbounded();
        let bridge = CloneableBridge::new(to_sub_tx, from_sub_rx);

        let mut world = World::new();
        world.insert_resource(bridge);

        from_sub_tx
            .send(BridgeEvent::Action(Box::new(|w: &mut World| {
                w.remove_resource::<CloneableBridge>();
            })))
            .unwrap();

        sync_bridge_cloneable(&mut world); // must not crash / UAF
        assert!(
            world.try_resource::<CloneableBridge>().is_none(),
            "the incoming Action removed the bridge resource"
        );
    }
}
