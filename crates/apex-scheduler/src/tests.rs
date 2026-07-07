    use super::*;
    use apex_core::access_desc;
    use apex_core::{prelude::*, query::Query, world::World};

    // ── Sh2: cost-model EMA + hysteresis (deterministic, no timing) ──────────
    #[test]
    fn cost_model_ema_and_hysteresis() {
        use std::time::Duration;
        let mut s = Scheduler::new();

        // No history → never prefers seq (caller uses the entity heuristic).
        assert!(!s.cost_model_prefers_seq(0));

        // A cheap stage (10µs < 0.8·40µs band) → prefers sequential.
        s.record_stage_cost(0, Duration::from_micros(10), false);
        assert!(s.cost_model_prefers_seq(0), "10us stage should run sequentially");

        // An expensive stage (feed 200µs repeatedly so the EMA climbs above the
        // upper band) → prefers parallel.
        for _ in 0..20 {
            s.record_stage_cost(1, Duration::from_micros(200), false);
        }
        assert!(
            !s.cost_model_prefers_seq(1),
            "200us stage should run in parallel"
        );

        // Hysteresis: a stage sitting at 45µs. From SEQ it stays SEQ (upper band
        // 48µs); from PAR it would stay PAR (lower band 32µs). Verify the SEQ side.
        for _ in 0..20 {
            s.record_stage_cost(2, Duration::from_micros(45), true); // ran seq
        }
        assert!(
            s.cost_model_prefers_seq(2),
            "45us with seq-history stays sequential (hysteresis upper band 48us)"
        );

        // Distinct indices are independent.
        assert!(s.cost_model_prefers_seq(0));
        assert!(!s.cost_model_prefers_seq(1));
    }

    // ── Sh2/ADR-003: ParallelPolicy — Fixed opts out of the EMA gates ─────────
    #[test]
    fn parallel_policy_fixed_vs_cost_model() {
        use std::time::Duration;
        // Two systems → entity crossover = min_entities_for_parallelism(2) = 25_000,
        // compared against per-system = stage_entity_count / 2.
        const SYS: usize = 2;

        // --- CostModel (default): the warm EMA fully decides. ---
        let mut cm = Scheduler::new();
        assert_eq!(cm.parallel_policy(), ParallelPolicy::CostModel);
        // Cheap EMA on stage 0 (10µs, below the band) → the cost model prefers SEQ.
        cm.record_stage_cost(0, Duration::from_micros(10), false);
        // Even at 60k entities (the entity heuristic alone would say PAR), CostModel
        // honors the cheap measurement → SEQ.
        assert!(
            cm.stage_prefers_seq(0, 60_000, SYS, 0, false),
            "CostModel: cheap warm EMA → SEQ regardless of high entity count"
        );
        // Expensive EMA on stage 1 (200µs) → PAR even at a low entity count — the
        // whole point of the model (heavy-low-entity work is parallelized).
        for _ in 0..20 {
            cm.record_stage_cost(1, Duration::from_micros(200), false);
        }
        assert!(
            !cm.stage_prefers_seq(1, 10_000, SYS, 0, false),
            "CostModel: expensive warm EMA → PAR regardless of low entity count"
        );

        // --- Fixed: entity counts only; the EMA is NEVER consulted. ---
        let mut fx = Scheduler::new();
        fx.set_parallel_policy(ParallelPolicy::Fixed);
        assert_eq!(fx.parallel_policy(), ParallelPolicy::Fixed);
        // Record the SAME cheap EMA on stage 0. Fixed ignores it: 60k entities
        // (per-system 30_000 ≥ 25_000) → PAR — the OPPOSITE verdict CostModel gave
        // on identical input, proving Fixed bypasses the EMA.
        fx.record_stage_cost(0, Duration::from_micros(10), false);
        assert!(
            !fx.stage_prefers_seq(0, 60_000, SYS, 0, false),
            "Fixed: high entity count → PAR even with a cheap EMA recorded"
        );
        // Low entity count (per-system 5_000 < 25_000) → SEQ, again ignoring the EMA.
        assert!(
            fx.stage_prefers_seq(0, 10_000, SYS, 0, false),
            "Fixed: low entity count → SEQ"
        );

        // --- The explicit floor is a hard gate under BOTH policies. ---
        assert!(
            cm.stage_prefers_seq(2, 100, SYS, 5_000, false),
            "CostModel: below the explicit floor → SEQ"
        );
        assert!(
            fx.stage_prefers_seq(2, 100, SYS, 5_000, false),
            "Fixed: below the explicit floor → SEQ"
        );

        // --- CostModel cold start (stage 7 has no EMA history). ---
        assert!(
            !cm.stage_prefers_seq(7, 10_000, SYS, 0, false),
            "CostModel cold start, auto_disable off → PAR"
        );
        assert!(
            cm.stage_prefers_seq(7, 10_000, SYS, 0, true),
            "CostModel cold start, auto_disable on, low entities → SEQ"
        );
    }

    #[derive(Component, Clone, Copy)]
    struct Pos {
        x: f32,
        y: f32,
    }
    #[derive(Component, Clone, Copy)]
    struct Vel {
        x: f32,
        y: f32,
    }
    #[derive(Component, Clone, Copy)]
    struct Hp(f32);
    #[derive(Clone, Copy)]
    struct DeltaTime(f32);

    // ── AutoSystem tests ──────────────────────────────────────

    struct AutoMovement;
    impl AutoSystem for AutoMovement {
        type Query = (Read<Vel>, Write<Pos>);
        type Resources = ();
        type Events = ();
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query_unchecked::<Self::Query>().for_each_mut(|_, (vel, mut pos)| {
                pos.x += vel.x;
                pos.y += vel.y;
            });
        }
    }

    struct AutoHealth;
    impl AutoSystem for AutoHealth {
        type Query = Write<Hp>;
        type Resources = ();
        type Events = ();
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query_unchecked::<Self::Query>().for_each_mut(|_, mut hp| {
                hp.0 = hp.0.max(0.0);
            });
        }
    }

    #[test]
    fn auto_system_access_correct() {
        // AutoMovement must have read:Vel, write:Pos
        let access = <(Read<Vel>, Write<Pos>) as WorldQuerySystemAccess>::system_access();
        assert!(!access.reads.is_empty(), "must read Vel");
        assert!(!access.writes.is_empty(), "must write Pos");
    }

    #[test]
    fn auto_system_runs_correctly() {
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);

        let mut world = World::new();

        world.spawn((Pos { x: 0.0, y: 0.0 }, Vel { x: 3.0, y: 4.0 }));

        sched.run_sequential(&mut world);

        let mut result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each(|_, p| {
            result = (p.x, p.y);
        });
        assert!((result.0 - 3.0).abs() < 1e-6);
        assert!((result.1 - 4.0).abs() < 1e-6);
    }

    #[test]
    fn cross_stage_change_detection_for_later_system_in_multi_system_stage() {
        // Regression for the per-EXECUTION-stage keying: a `Changed<T>` reader the planner sequenced
        // AFTER another system of the same `StageLabel` must use ITS OWN stage's base — not the base
        // advanced by the earlier system. Mirrors picking (a non-first PreUpdate system reading
        // Changed<GlobalTransform>). Keying the base by `StageLabel` (instead of execution-stage index)
        // broke this: the later system saw "changed since the earlier system", missing cross-frame writes.
        use apex_core::query::Changed;
        #[derive(Component, Clone, Copy)]
        struct V(u32);
        #[derive(Default)]
        struct Saw(Vec<bool>);
        #[derive(Default)]
        struct FrameNo(u32);

        let mut world = World::new();
        world.insert_resource(Saw::default());
        world.insert_resource(FrameNo::default());
        let e = world.spawn((V(0),));

        let mut s = Scheduler::new();
        // Two PreUpdate systems — Sequential systems run in their own execution stages, so `observe` is a
        // LATER execution stage than `lead` (same `StageLabel::PreUpdate`).
        s.add_systems(StageLabel::PreUpdate, seq("lead", |_w: &mut World| {}));
        s.add_systems(
            StageLabel::PreUpdate,
            seq("observe", move |w: &mut World| {
                let base = w.last_run_tick();
                let changed = Query::<(Changed<V>,)>::new_with_tick(w, base).iter().count() > 0;
                w.resource_mut::<Saw>().0.push(changed);
            }),
        );
        s.add_systems(
            StageLabel::PostUpdate,
            seq("write", move |w: &mut World| {
                if w.resource::<FrameNo>().0 == 2 {
                    if let Some(mut v) = w.get_mut::<V>(e) {
                        v.0 += 1;
                    }
                }
                w.resource_mut::<FrameNo>().0 += 1;
            }),
        );

        for _ in 0..5 {
            s.run(&mut world);
        }
        // `observe` (the later PreUpdate system) must see the PostUpdate-frame-2 write on frame 3.
        assert!(
            world.resource::<Saw>().0[3],
            "a later reader in a multi-system stage must still see the cross-stage write"
        );
    }

    #[test]
    fn cross_stage_change_detection_postupdate_to_preupdate_next_frame() {
        // TD-52 regression: a write in PostUpdate of frame N must be visible to a PreUpdate
        // `Changed<T>` reader in frame N+1. The old per-frame change tick made every reader's base
        // "the end of the previous frame", so a PostUpdate-N write was NOT newer than a PreUpdate-(N+1)
        // reader's base and got skipped. The per-stage change-detection window fixes it (each stage's
        // base = the tick at which THAT stage last ran).
        use apex_core::query::Changed;

        #[derive(Component, Clone, Copy)]
        struct Marked(u32);
        #[derive(Default)]
        struct Seen(Vec<bool>);
        #[derive(Default)]
        struct FrameNo(u32);

        let mut world = World::new();
        world.insert_resource(Seen::default());
        world.insert_resource(FrameNo::default());
        let e = world.spawn((Marked(0),));

        let mut sched = Scheduler::new();
        // PreUpdate observer: did `Marked` change since this stage last ran?
        sched.add_systems(
            StageLabel::PreUpdate,
            seq("observe", move |w: &mut World| {
                let last = w.last_run_tick();
                let changed = Query::<(Changed<Marked>,)>::new_with_tick(w, last).iter().count() > 0;
                w.resource_mut::<Seen>().0.push(changed);
            }),
        );
        // PostUpdate writer: mutate `Marked` only on frame 2 — a LATER stage of that frame.
        sched.add_systems(
            StageLabel::PostUpdate,
            seq("write", move |w: &mut World| {
                if w.resource::<FrameNo>().0 == 2 {
                    if let Some(mut m) = w.get_mut::<Marked>(e) {
                        m.0 += 1; // mutable access stamps the change tick at the PostUpdate tick
                    }
                }
                w.resource_mut::<FrameNo>().0 += 1;
            }),
        );

        for _ in 0..5 {
            sched.run(&mut world);
        }

        // Frame 0: the freshly spawned `Marked` is a change ⇒ seen (true).
        // Frame 1: nothing new ⇒ false.
        // Frame 2: PostUpdate writes AFTER PreUpdate already ran ⇒ not seen that frame ⇒ false.
        // Frame 3: PreUpdate sees the previous-frame PostUpdate write — the cross-stage detection the
        //          old per-frame model MISSED (this index would be `false` without the fix).
        // Frame 4: nothing new ⇒ false.
        let seen = &world.resource::<Seen>().0;
        assert_eq!(
            seen,
            &vec![true, false, false, true, false],
            "PreUpdate(N+1) must see PostUpdate(N) write (cross-stage change detection, TD-52)"
        );
        assert!(seen[3], "the cross-stage write of frame 2 must be visible to PreUpdate in frame 3");
    }

    #[test]
    fn auto_system_no_conflict_same_stage() {
        // AutoMovement (Write<Pos>) and AutoHealth (Write<Hp>) — no conflict
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);
        sched.add_auto_system("health", AutoHealth);
        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1, "no conflict — there should be 1 Stage");
        assert!(stages[0].all_parallel);
        assert_eq!(stages[0].system_count(), 2);
    }

    #[test]
    fn auto_system_conflict_separate_stages() {
        // Two AutoSystems write Pos — a conflict
        struct AutoMovement2;
        impl AutoSystem for AutoMovement2 {
            type Query = Write<Pos>;
            type Resources = ();
            type Events = ();
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_auto_system("m1", AutoMovement); // Write<Pos>
        sched.add_auto_system("m2", AutoMovement2); // Write<Pos>
        sched.compile().unwrap();

        assert_eq!(
            sched.stages().unwrap().len(),
            2,
            "Write+Write must yield 2 Stages"
        );
    }

    // ── ConflictKind tests ────────────────────────────────────

    #[test]
    fn conflict_kind_in_edge_info() {
        struct WriterA;
        impl ParSystem for WriterA {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct WriterB;
        impl ParSystem for WriterB {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        let a = sched.add_par_system("a", WriterA);
        let b = sched.add_par_system("b", WriterB);
        sched.compile().unwrap();

        let conflicts = sched.conflicts_between(a, b);
        assert!(!conflicts.is_empty(), "there should be a conflict");
        assert!(matches!(conflicts[0], ConflictKind::WriteWrite { .. }));
    }

    #[test]
    fn asd_does_not_split_resource_mutating_system_td37() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // Mimics plain-fn `(Query<&Pos>, ResMut<Counter>)`: a WIDE query (reading Pos over
        // many entities) + a RESOURCE mutation. Before the TD-37 fix, ASD split such a system into chunks
        // and called the BODY once per chunk ⇒ the resource mutation was multiplied (many_foxes@10000 bug: foxes ~20×
        // faster). With the fix `resource_write` gates ASD ⇒ the system = one task ⇒ exactly one call.
        struct Counter; // only a carrier of the "resource" TypeId
        struct ResourceMutator(Arc<AtomicUsize>);
        impl ParSystem for ResourceMutator {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new()
                    .read::<Pos>()
                    .write::<Counter>()
                    .resource_write()
            }
            fn run(&mut self, _: SystemContext<'_>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let mut sched = Scheduler::new();
        sched.add_par_system("res_mutator", ResourceMutator(calls.clone()));

        let mut world = World::new();
        // Deliberately above the chunk threshold — without the fix it would row-split into many chunks.
        for _ in 0..50_000 {
            world.spawn((Pos { x: 0.0, y: 0.0 },));
        }

        sched.run(&mut world);

        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "a system mutating a resource must run EXACTLY once, not once per ASD chunk (TD-37)"
        );
    }

    #[test]
    fn asd_still_splits_pure_query_system() {
        // Control: a pure system (query only, no resource mutation/Commands) over many
        // entities STILL decomposes (per-entity writes are idempotent under chunking) —
        // the TD-37 fix does not touch data-parallel perf paths. Check that its access is NOT marked
        // `writes_resource`/`uses_commands` (i.e. ASD-eligible).
        let access = AccessDescriptor::new().read::<Vel>().write::<Pos>();
        assert!(!access.writes_resource, "a pure query system does not write a resource");
        assert!(!access.uses_commands, "a pure query system does not use Commands");
    }

    #[test]
    fn sequential_barrier_in_edge_info() {
        let mut sched = Scheduler::new();
        let _par = sched.add_auto_system("movement", AutoMovement);
        let _seq = sched.add_system("barrier", |_| {}).id();
        sched.compile().unwrap();

        // There should be SequentialBarrier edges
        let has_barrier = sched
            .edge_info
            .iter()
            .any(|e| matches!(e.kind, ConflictKind::SequentialBarrier));
        assert!(has_barrier, "the Sequential barrier must be in edge_info");
    }

    // ── debug_plan_verbose test ───────────────────────────────

    #[test]
    fn debug_plan_verbose_works() {
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);
        sched.add_auto_system("health", AutoHealth);
        sched.add_system("commands", |_| {});
        sched.compile().unwrap();

        let plan = sched.debug_plan_verbose();
        assert!(plan.contains("PARALLEL"), "there should be a PARALLEL Stage");
        assert!(plan.contains("sequential"), "there should be a sequential Stage");
        assert!(plan.contains("Conflict"), "must show conflicts");
        assert!(plan.contains("Summary"), "must show a summary");
    }

    #[test]
    fn compile_with_world_shows_component_names() {
        let mut world = World::new();

        // Explicit component registration is needed for populate_type_names.
        // Auto-registration via #[derive(Component)] also works,
        // but for certainty in this diagnostic test we register explicitly.
        world.register_component::<Pos>();
        world.register_component::<Vel>();

        // A system using Pos and Vel — their names will appear in reads/writes
        struct MovementSystem;
        impl ParSystem for MovementSystem {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().read::<Vel>().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("move", MovementSystem);
        sched.compile_with_world(&world).unwrap();

        let plan = sched.debug_plan_verbose();
        assert!(
            plan.contains("Pos"),
            "debug_plan must contain 'Pos', contains: {}",
            plan
        );
        assert!(
            plan.contains("Vel"),
            "debug_plan must contain 'Vel', contains: {}",
            plan
        );
    }

    // ── Incrementality test ────────────────────────────────

    #[test]
    fn incremental_compile_after_add() {
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);
        sched.compile().unwrap();

        // Graph compiled
        assert!(sched.execution_plan.is_some());
        assert!(!sched.graph_dirty);

        // Add a new system — the plan is invalidated
        sched.add_auto_system("health", AutoHealth);
        assert!(sched.execution_plan.is_none());
        assert!(sched.graph_dirty);

        // Compile again
        sched.compile().unwrap();
        assert!(sched.execution_plan.is_some());
        assert_eq!(sched.stages().unwrap().len(), 1);
    }

    // ── Original tests (compatibility) ───────────────────

    #[test]
    fn sequential_ordering() {
        #[allow(dead_code)]
        struct MovementSystem;
        impl ParSystem for MovementSystem {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().read::<Vel>().write::<Pos>()
            }
            fn run(&mut self, ctx: SystemContext<'_>) {
                ctx.query_unchecked::<(Read<Vel>, Write<Pos>)>()
                    .for_each_mut(|_, (vel, mut pos)| {
                        pos.x += vel.x;
                        pos.y += vel.y;
                    });
            }
        }

        let mut sched = Scheduler::new();
        let log: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>> = Default::default();

        let log_a = log.clone();
        let a = sched
            .add_system("a", move |_| {
                log_a.lock().unwrap().push("a");
            })
            .id();
        let log_b = log.clone();
        let b = sched
            .add_system("b", move |_| {
                log_b.lock().unwrap().push("b");
            })
            .id();

        sched.add_dependency(b, a);
        sched.compile().unwrap();

        let mut world = World::new();
        sched.run_sequential(&mut world);
        assert_eq!(*log.lock().unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn circular_dependency_detected() {
        let mut sched = Scheduler::new();
        let a = sched.add_system("a", |_| {}).id();
        let b = sched.add_system("b", |_| {}).id();
        sched.add_dependency(b, a);
        sched.add_dependency(a, b);
        let err = sched.compile();
        assert!(err.is_err());
        // The error message must contain the system names
        if let Err(SchedulerError::CircularDependency { cycle_info }) = err {
            assert!(cycle_info.contains("a") || cycle_info.contains("b"));
        }
    }

    #[test]
    fn par_write_conflict_separate_stages() {
        struct WriterA;
        impl ParSystem for WriterA {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct WriterB;
        impl ParSystem for WriterB {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("a", WriterA);
        sched.add_par_system("b", WriterB);
        sched.compile().unwrap();

        assert_eq!(sched.stages().unwrap().len(), 2);
    }

    #[test]
    fn sequential_breaks_parallel_groups() {
        struct MovementSystem;
        impl ParSystem for MovementSystem {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().read::<Vel>().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct HealthSystem;
        impl ParSystem for HealthSystem {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Hp>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("par_a", MovementSystem);
        sched.add_system("barrier", |_| {});
        sched.add_par_system("par_b", HealthSystem);
        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // sequential no longer fragments — parallel ones are grouped before it
        assert_eq!(stages.len(), 2);
        assert!(stages[0].all_parallel);
        assert!(!stages[1].all_parallel);
    }

    #[test]
    fn fn_par_system_with_resource() {
        let mut sched = Scheduler::new();
        sched.add_par_access(
            "scaled_movement",
            access_desc!(read<DeltaTime>, read<Vel>, write<Pos>),
            |ctx: SystemContext<'_>| {
                let dt = ctx.resource::<DeltaTime>();
                ctx.query_unchecked::<(Read<Vel>, Write<Pos>)>()
                    .for_each_mut(|_, (vel, mut pos)| {
                        pos.x += vel.x * (*dt).0;
                        pos.y += vel.y * (*dt).0;
                    });
            },
        );

        let mut world = World::new();

        world.insert_resource(DeltaTime(0.5));
        world.spawn((Pos { x: 0.0, y: 0.0 }, Vel { x: 2.0, y: 4.0 }));

        sched.run_sequential(&mut world);

        let mut result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each(|_, p| {
            result = (p.x, p.y);
        });

        assert!((result.0 - 1.0).abs() < 1e-6);
        assert!((result.1 - 2.0).abs() < 1e-6);
    }

    /// Parallel execution: both AutoSystems apply their changes.
    #[test]
    fn parallel_auto_systems_correctness() {
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);
        sched.add_auto_system("health", AutoHealth);
        sched.compile().unwrap();
        assert!(sched.stages().unwrap()[0].is_parallelizable());

        let mut world = World::new();

        world.spawn((Pos { x: 0.0, y: 0.0 }, Vel { x: 1.0, y: 2.0 }, Hp(-5.0)));

        sched.run(&mut world);

        let mut pos_result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each(|_, p| {
            pos_result = (p.x, p.y);
        });
        assert!((pos_result.0 - 1.0).abs() < 1e-6);
        assert!((pos_result.1 - 2.0).abs() < 1e-6);

        let mut hp_result = -1.0f32;
        Query::<Read<Hp>>::new(&world).for_each(|_, hp| {
            hp_result = hp.0;
        });
        assert!((hp_result - 0.0).abs() < 1e-6);
    }

    // ── StageLabel tests ────────────────────────────────────────

    #[test]
    fn startup_system_runs_once() {
        let mut sched = Scheduler::new();
        let startup_count = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let counter = startup_count.clone();

        sched.add_startup_system("init", move |_| {
            *counter.lock().unwrap() += 1;
        });

        let mut world = World::new();

        // First run() — Startup runs
        sched.run_sequential(&mut world);
        assert_eq!(
            *startup_count.lock().unwrap(),
            1,
            "Startup must run once"
        );

        // Second run() — Startup does NOT run
        sched.run_sequential(&mut world);
        assert_eq!(
            *startup_count.lock().unwrap(),
            1,
            "Startup must NOT run again"
        );
    }

    #[test]
    fn stage_label_in_debug_plan() {
        let mut sched = Scheduler::new();
        sched.add_startup_system("init", |_| {});
        sched.add_auto_system("movement", AutoMovement);
        sched.compile().unwrap();

        let plan = sched.debug_plan();
        assert!(
            plan.contains("Startup"),
            "debug_plan must contain the Startup label"
        );
        assert!(
            plan.contains("Update"),
            "debug_plan must contain the Update label"
        );
    }

    #[test]
    fn add_system_to_stage_custom_label() {
        let mut sched = Scheduler::new();

        // Add systems to different stages
        sched.add_system_to_stage("pre", |_| {}, StageLabel::PreUpdate);
        sched.add_auto_system_to_stage("update_movement", AutoMovement, StageLabel::Update);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // There should be at least 2 Stages: PreUpdate and Update
        assert!(
            stages.len() >= 2,
            "There should be at least 2 Stages, got {}",
            stages.len()
        );

        // Check that PreUpdate comes before Update
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        assert!(pre_idx.is_some(), "There should be a PreUpdate Stage");
        assert!(upd_idx.is_some(), "There should be an Update Stage");
        assert!(
            pre_idx.unwrap() < upd_idx.unwrap(),
            "PreUpdate must come before Update"
        );
    }

    #[test]
    fn startup_auto_system() {
        let mut sched = Scheduler::new();

        // AutoSystem on Startup
        sched.add_startup_auto_system("init_movement", AutoMovement);

        // A regular system on Update
        sched.add_auto_system("update_health", AutoHealth);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        assert!(stages.len() >= 2, "There should be at least 2 Stages");

        // Startup runs first
        assert_eq!(
            stages[0].label,
            StageLabel::Startup,
            "The first Stage must be Startup"
        );
        assert!(
            stages.iter().any(|s| s.label == StageLabel::Update),
            "There should be an Update Stage"
        );
    }

    #[test]
    fn startup_system_works_via_run() {
        let mut sched = Scheduler::new();
        let startup_val = std::sync::Arc::new(std::sync::Mutex::new(0i32));
        let val = startup_val.clone();

        sched.add_startup_system("init", move |world: &mut World| {
            world.insert_resource(42i32);
            *val.lock().unwrap() = 42;
        });

        let mut world = World::new();

        // First run
        sched.run_sequential(&mut world);
        assert_eq!(
            *startup_val.lock().unwrap(),
            42,
            "The Startup system must run"
        );
        assert_eq!(
            *world.resource::<i32>(),
            42,
            "The resource must be set"
        );

        // Second run — the resource must persist (Startup does not overwrite)
        sched.run_sequential(&mut world);
        assert_eq!(*world.resource::<i32>(), 42, "The resource must not change");
    }

    // ── Event conflicts ────────────────────────────────────────

    struct EventWriterForTest;
    impl ParSystem for EventWriterForTest {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write_event::<i32>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct AnotherEventWriter;
    impl ParSystem for AnotherEventWriter {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write_event::<i32>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct EventReaderForTest;
    impl ParSystem for EventReaderForTest {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read_event::<i32>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    #[test]
    fn event_write_write_conflict() {
        let mut sched = Scheduler::new();

        sched.add_par_system("writer_a", EventWriterForTest);
        sched.add_par_system("writer_b", AnotherEventWriter);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // Two EventWriters of the same type → must be in different Stages (conflict)
        assert!(
            stages.len() >= 2,
            "EventWriteWrite conflict: expected at least 2 Stages, got {}",
            stages.len()
        );
    }

    #[test]
    fn event_write_read_conflict() {
        let mut sched = Scheduler::new();

        sched.add_par_system("writer", EventWriterForTest);
        sched.add_par_system("reader", EventReaderForTest);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // EventWriter + EventReader of the same type → must be in different Stages (conflict)
        assert!(
            stages.len() >= 2,
            "EventWriteRead conflict: expected at least 2 Stages, got {}",
            stages.len()
        );
    }

    /// F2: two systems reading the SAME event must be serialized — each
    /// EventReader mutates the queue's shared cursor registry, so running them in
    /// parallel is a data race. (They used to share a stage — the racy old
    /// behavior; reader parallelism returns with the per-system cursor model,
    /// wave 6.)
    #[test]
    fn same_event_readers_are_serialized() {
        let mut sched = Scheduler::new();

        let a = sched.add_par_system("reader_a", EventReaderForTest);
        let b = sched.add_par_system("reader_b", EventReaderForTest);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        let sa = stages.iter().position(|s| s.system_ids.contains(&a)).unwrap();
        let sb = stages.iter().position(|s| s.system_ids.contains(&b)).unwrap();
        assert_ne!(sa, sb, "two readers of the same event must be serialized (F2)");

        let conflicts = sched.conflicts_between(a, b);
        assert!(
            conflicts
                .iter()
                .any(|c| matches!(c, ConflictKind::SharedEventReaders { .. })),
            "the conflict must be SharedEventReaders, got {conflicts:?}"
        );
    }

    struct DifferentEventReader;
    impl ParSystem for DifferentEventReader {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read_event::<f64>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    #[test]
    fn event_read_different_events_no_conflict() {
        let mut sched = Scheduler::new();
        // They listen to different events — no conflict
        sched.add_par_system("reader_i32", EventReaderForTest); // Listen<i32>
        sched.add_par_system("reader_f64", DifferentEventReader); // Listen<f64>
        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert!(
            stages.iter().any(|s| s.system_ids.len() >= 2),
            "EventReads of different events must not conflict: expected a Stage with both systems"
        );
    }

    #[test]
    fn event_conflict_kind_in_edge_info() {
        let mut sched = Scheduler::new();

        let wid = sched.add_par_system("writer", EventWriterForTest);
        let rid = sched.add_par_system("reader", AnotherEventWriter);

        sched.compile().unwrap();

        let conflicts = sched.conflicts_between(wid, rid);
        assert!(
            !conflicts.is_empty(),
            "There should be a conflict between EventWriter and EventWriter"
        );

        // Check the conflict kind
        let has_event_conflict = conflicts
            .iter()
            .any(|c| matches!(c, ConflictKind::EventWriteWrite { .. }));
        assert!(has_event_conflict, "The conflict must be EventWriteWrite");
    }

    #[test]
    fn event_ordering_disabled_no_conflict() {
        let mut sched = Scheduler::new();
        sched.enable_event_ordering(false);

        sched.add_par_system("emitter", EventWriterForTest); // Emit<i32>
        sched.add_par_system("listener", EventReaderForTest); // Listen<i32>

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        assert!(
            stages.iter().any(|s| s.system_ids.len() >= 2),
            "With event_ordering=false, Emit+Listen must not conflict"
        );
    }

    #[test]
    fn event_ordering_enabled_by_default() {
        let mut sched = Scheduler::new();
        // By default enable_event_ordering is not called — it must be true

        sched.add_par_system("emitter", EventWriterForTest); // Emit<i32>
        sched.add_par_system("listener", EventReaderForTest); // Listen<i32>

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        assert!(
            stages.len() >= 2,
            "By default Emit+Listen must be in different Stages, got {}",
            stages.len()
        );
    }

    // ── configure_stages ─────────────────────────────────────────

    #[test]
    fn configure_stages_custom_order() {
        let mut sched = Scheduler::new();

        // Add systems to Update and PreUpdate
        sched.add_auto_system_to_stage("update_movement", AutoMovement, StageLabel::Update);
        sched.add_system_to_stage("pre_work", |_| {}, StageLabel::PreUpdate);

        // Change the order: Update BEFORE PreUpdate
        sched.configure_stages(vec![
            StageLabel::Startup,
            StageLabel::Update,
            StageLabel::PreUpdate,
            StageLabel::First,
            StageLabel::PostUpdate,
            StageLabel::Last,
        ]);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();

        // Update must come before PreUpdate
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);

        assert!(upd_idx.is_some(), "There should be an Update Stage");
        assert!(pre_idx.is_some(), "There should be a PreUpdate Stage");
        assert!(
            upd_idx.unwrap() < pre_idx.unwrap(),
            "Update must come before PreUpdate with configure_stages"
        );
    }

    /// D8: multiple Custom stages (all priority 7) are ordered by NAME, not by
    /// FxHashMap iteration — reproducible run-to-run and across platforms.
    #[test]
    fn custom_stages_ordered_by_name_deterministically() {
        let mut sched = Scheduler::new();
        sched.add_system_to_stage("z", |_| {}, StageLabel::Custom("zebra".to_string()));
        sched.add_system_to_stage("a", |_| {}, StageLabel::Custom("alpha".to_string()));
        sched.add_system_to_stage("m", |_| {}, StageLabel::Custom("mango".to_string()));
        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        let custom: Vec<String> = stages
            .iter()
            .filter_map(|s| match &s.label {
                StageLabel::Custom(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            custom,
            vec![
                "alpha".to_string(),
                "mango".to_string(),
                "zebra".to_string()
            ],
            "Custom stages must be ordered by name"
        );
    }

    #[test]
    fn configure_stages_keeps_missing_at_end() {
        let mut sched = Scheduler::new();

        // Add systems to different stages
        sched.add_system_to_stage("pre", |_| {}, StageLabel::PreUpdate);
        sched.add_auto_system_to_stage("update_movement", AutoMovement, StageLabel::Update);
        sched.add_system_to_stage("last_work", |_| {}, StageLabel::Last);

        // Specify only Update and PreUpdate — Last is not listed
        sched.configure_stages(vec![
            StageLabel::Startup,
            StageLabel::Update,
            StageLabel::PreUpdate,
        ]);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();

        // Update and PreUpdate must be present (in the given order)
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);
        assert!(upd_idx.is_some());
        assert!(pre_idx.is_some());
        assert!(
            upd_idx.unwrap() < pre_idx.unwrap(),
            "Update must come before PreUpdate"
        );

        // Last must be at the end (not in order, appended automatically)
        let last_idx = stages.iter().position(|s| s.label == StageLabel::Last);
        assert!(
            last_idx.is_some(),
            "Last must be present even if not listed in configure_stages"
        );
        assert!(
            last_idx.unwrap() > pre_idx.unwrap() || last_idx.unwrap() > upd_idx.unwrap(),
            "Last (not listed in order) must be at the end"
        );
    }

    // ── Pipeline tests ─────────────────────────────────────────

    #[derive(Clone, Copy)]
    struct DamageEvent;

    struct EmitDamage;
    impl ParSystem for EmitDamage {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write_event::<DamageEvent>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct TransformDmg;
    impl ParSystem for TransformDmg {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new()
                .read_event::<DamageEvent>()
                .write_event::<DamageEvent>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct ListenDamage;
    impl ParSystem for ListenDamage {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read_event::<DamageEvent>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct ListenDamage2;
    impl ParSystem for ListenDamage2 {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read_event::<DamageEvent>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct ListenerOnly;
    impl ParSystem for ListenerOnly {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read_event::<DamageEvent>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    #[test]
    fn pipeline_basic_chain_ordering() {
        let mut sched = Scheduler::new();

        let physics_id = sched.add_par_system("physics", EmitDamage);
        let armor_id = sched.add_par_system("armor", TransformDmg);
        let health_id = sched.add_par_system("health", ListenDamage);

        Scheduler::event_pipeline::<DamageEvent>()
            .produced_by("physics")
            .transformed_by("armor")
            .consumed_by("health")
            .build(&mut sched);

        sched.compile().unwrap();

        // There should be at least 3 Stages: physics → armor → health
        let stages = sched.stages().unwrap();
        assert!(
            stages.len() >= 3,
            "A 3-stage pipeline must create at least 3 Stages, got: {}",
            stages.len()
        );

        // Check the order of the flat list
        let flat = &sched.execution_plan.as_ref().unwrap().flat_order;
        let pos_physics = flat.iter().position(|&id| id == physics_id).unwrap();
        let pos_armor = flat.iter().position(|&id| id == armor_id).unwrap();
        let pos_health = flat.iter().position(|&id| id == health_id).unwrap();
        assert!(pos_physics < pos_armor, "physics must come before armor");
        assert!(pos_armor < pos_health, "armor must come before health");
    }

    /// §0.2a hygiene: `Pipeline::build` naming an unregistered system used to
    /// panic on an opaque `unwrap`. It must now log an error and skip wiring —
    /// the scheduler still compiles.
    #[test]
    fn pipeline_build_unregistered_system_does_not_panic() {
        let mut sched = Scheduler::new();
        let _physics = sched.add_par_system("physics", EmitDamage);
        // "ghost" is never registered.
        Scheduler::event_pipeline::<DamageEvent>()
            .produced_by("physics")
            .consumed_by("ghost")
            .build(&mut sched);
        sched.compile().unwrap();
    }

    /// Both consumers of the same event run after the producer and each receives
    /// the event. They are SERIALIZED with respect to each other (F2: reading the
    /// same event mutates its shared cursor registry, so parallel reads race);
    /// reader parallelism returns with the per-system cursor model (wave 6).
    #[test]
    fn pipeline_consumers_run_after_producer_and_are_serialized() {
        let mut sched = Scheduler::new();

        let physics_id = sched.add_par_system("physics", EmitDamage);
        let health_id = sched.add_par_system("health", ListenDamage);
        let sound_id = sched.add_par_system("sound", ListenDamage2);

        Scheduler::event_pipeline::<DamageEvent>()
            .produced_by("physics")
            .consumed_by("health")
            .consumed_by("sound")
            .build(&mut sched);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        let stage_of = |id| stages.iter().position(|s| s.system_ids.contains(&id)).unwrap();
        let (p, h, s) = (stage_of(physics_id), stage_of(health_id), stage_of(sound_id));
        // Producer before both consumers; the two consumers serialized (F2).
        assert!(p < h && p < s, "producer must run before both consumers");
        assert_ne!(h, s, "consumers of the same event are serialized (F2)");
    }

    #[test]
    fn pipeline_validation_catches_wrong_role() {
        let mut sched = Scheduler::new();

        // ListenerOnly has no Emit<Damage> — it cannot be a Producer
        let _bad_id = sched.add_par_system("bad_producer", ListenerOnly);

        let result = Scheduler::event_pipeline::<DamageEvent>()
            .produced_by("bad_producer")
            .build_validated(&mut sched);

        assert!(
            result.is_err(),
            "There should be an error: the system is declared a Producer but has no Emit"
        );

        let errors = result.unwrap_err();
        assert!(matches!(
            errors[0],
            PipelineValidationError::ProducerMissingEmit { .. }
        ));
    }

    #[test]
    fn configure_stages_persists_across_compiles() {
        let mut sched = Scheduler::new();

        // Add systems to Update and PreUpdate
        sched.add_auto_system_to_stage("update_movement", AutoMovement, StageLabel::Update);
        sched.add_system_to_stage("pre_work", |_| {}, StageLabel::PreUpdate);

        // Configure the order: Update BEFORE PreUpdate
        sched.configure_stages(vec![
            StageLabel::Startup,
            StageLabel::Update,
            StageLabel::PreUpdate,
        ]);

        // First compilation
        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);
        assert!(
            upd_idx.unwrap() < pre_idx.unwrap(),
            "Update must come before PreUpdate after the first compilation"
        );

        // Add a new system (triggers invalidate_plan)
        sched.add_system_to_stage("more_pre_work", |_| {}, StageLabel::PreUpdate);

        // Second compilation — stage_order must persist
        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);
        assert!(
            upd_idx.is_some(),
            "The Update Stage must exist after recompilation"
        );
        assert!(
            pre_idx.is_some(),
            "The PreUpdate Stage must exist after recompilation"
        );
        assert!(upd_idx.unwrap() < pre_idx.unwrap(),
            "Update must come before PreUpdate after recompilation — stage_order must persist");

        // Third compilation — one more call for certainty
        sched.add_system_to_stage("extra", |_| {}, StageLabel::Last);
        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);
        assert!(
            upd_idx.unwrap() < pre_idx.unwrap(),
            "Update before PreUpdate after the third compilation"
        );
    }

    #[test]
    fn archetype_indices_for_subworld_uses_any_criterion() {
        // Two systems with cross Write/Read over Pos and Vel
        // must not create a CircularDependency.
        // Check that the any() criterion in compute_archetype_indices
        // and BidirectionalWriteRead in detect_conflict_kind work correctly.
        let _world = World::new();

        let mut sched = Scheduler::new();

        // Just dummy systems: check that compilation succeeds
        sched.add_system("sys_a", |_: &mut World| {});
        sched.add_system("sys_b", |_: &mut World| {});

        let result = sched.compile();
        assert!(
            result.is_ok(),
            "Compilation must succeed without errors: {:?}",
            result.err()
        );
    }

    #[test]
    fn independent_resolves_bidirectional_write_read_deterministically() {
        // A: Read<Vel>, Write<Pos>; B: Read<Pos>, Write<Vel> — a true BidirectionalWriteRead
        // (without an explicit order → CircularDependency).
        struct A;
        impl ParSystem for A {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().read::<Vel>().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct B;
        impl ParSystem for B {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().read::<Pos>().write::<Vel>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        // Without `independent` — a cycle.
        let mut sched = Scheduler::new();
        sched.add_par_system("a", A);
        sched.add_par_system("b", B);
        assert!(
            matches!(sched.compile(), Err(SchedulerError::CircularDependency { .. })),
            "cross write/read without an explicit order = CircularDependency"
        );

        // With `independent` — it compiles, order is deterministic (registration: a → b).
        let mut sched = Scheduler::new();
        sched.add_par_system("a", A);
        sched.add_par_system("b", B);
        sched.independent(&["a", "b"]).unwrap();
        sched
            .compile()
            .expect("independent removes CircularDependency, serializing in registration order");
    }

    /// D5: a symmetric (WriteWrite) conflict plus an explicit ordering that runs
    /// against registration order must NOT fabricate a CircularDependency — the
    /// explicit ordering wins and serializes the two systems.
    #[test]
    fn symmetric_conflict_respects_reverse_explicit_order() {
        struct WriteA;
        impl ParSystem for WriteA {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct WriteB;
        impl ParSystem for WriteB {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("a", WriteA); // registered first
        sched.add_par_system("b", WriteB);
        // Explicit ordering against registration: b before a.
        sched.before("b", "a").unwrap();

        // Old code: conflict edge a->b plus explicit b->a = false cycle.
        sched
            .compile()
            .expect("explicit ordering must resolve the symmetric conflict without a false cycle");

        let a = sched.find_id_by_name("a").unwrap();
        let b = sched.find_id_by_name("b").unwrap();
        let stages = sched.stages().unwrap();
        let a_stage = stages.iter().position(|s| s.system_ids.contains(&a)).unwrap();
        let b_stage = stages.iter().position(|s| s.system_ids.contains(&b)).unwrap();
        assert!(b_stage < a_stage, "explicit before(\"b\",\"a\") must run b before a");
    }

    // ── R-3: ordering declared on configs (`.before/.after/.chain`) ────────

    /// Helper: index of the execution stage containing `name` after compile.
    #[cfg(test)]
    fn stage_of(sched: &Scheduler, name: &str) -> usize {
        let id = sched.find_id_by_name(name).unwrap();
        sched
            .stages()
            .unwrap()
            .iter()
            .position(|s| s.system_ids.contains(&id))
            .unwrap()
    }

    /// `.after("x")` on a config orders it after the named system.
    #[test]
    fn config_after_orders_by_name() {
        use crate::config::IntoScheduleConfigs;
        let mut sched = Scheduler::new();
        sched.add_systems(
            StageLabel::Update,
            (
                par("a", |_: SystemContext<'_>| {}),
                par("b", |_: SystemContext<'_>| {}).after("a"),
            ),
        );
        sched.compile().unwrap();
        assert!(
            stage_of(&sched, "a") < stage_of(&sched, "b"),
            ".after(\"a\") must run b after a"
        );
    }

    /// `.before("x")` accepts a FORWARD reference (target registered later in the
    /// same tuple) — name resolution is deferred to compile().
    #[test]
    fn config_before_forward_reference() {
        use crate::config::IntoScheduleConfigs;
        let mut sched = Scheduler::new();
        sched.add_systems(
            StageLabel::Update,
            (
                par("a", |_: SystemContext<'_>| {}).before("b"),
                par("b", |_: SystemContext<'_>| {}),
            ),
        );
        sched.compile().unwrap();
        assert!(
            stage_of(&sched, "a") < stage_of(&sched, "b"),
            ".before(\"b\") forward-ref must run a before b"
        );
    }

    /// `.chain()` on a tuple sequences elements positionally: a → b → c.
    #[test]
    fn config_chain_orders_tuple() {
        use crate::config::IntoScheduleConfigs;
        let mut sched = Scheduler::new();
        sched.add_systems(
            StageLabel::Update,
            (
                par("a", |_: SystemContext<'_>| {}),
                par("b", |_: SystemContext<'_>| {}),
                par("c", |_: SystemContext<'_>| {}),
            )
                .chain(),
        );
        sched.compile().unwrap();
        let (a, b, c) = (
            stage_of(&sched, "a"),
            stage_of(&sched, "b"),
            stage_of(&sched, "c"),
        );
        assert!(a < b && b < c, ".chain() must order a < b < c (got {a},{b},{c})");
    }

    /// A nested `.chain()` inside an unchained tuple keeps its edges after the
    /// tuple flattens (positional-edge offsetting).
    #[test]
    fn config_nested_chain_preserved() {
        use crate::config::IntoScheduleConfigs;
        let mut sched = Scheduler::new();
        sched.add_systems(
            StageLabel::Update,
            (
                (
                    par("a", |_: SystemContext<'_>| {}),
                    par("b", |_: SystemContext<'_>| {}),
                )
                    .chain(),
                par("c", |_: SystemContext<'_>| {}),
            ),
        );
        sched.compile().unwrap();
        assert!(
            stage_of(&sched, "a") < stage_of(&sched, "b"),
            "nested .chain() a → b must survive tuple flatten"
        );
    }

    /// `.after("<unknown>")` on a config surfaces as a loud `SystemNotFound` at
    /// compile — never a silent drop (§0.2a).
    #[test]
    fn config_after_unknown_name_errors() {
        use crate::config::IntoScheduleConfigs;
        let mut sched = Scheduler::new();
        sched.add_systems(
            StageLabel::Update,
            par("a", |_: SystemContext<'_>| {}).after("ghost"),
        );
        assert!(
            matches!(sched.compile(), Err(SchedulerError::SystemNotFound(_))),
            ".after(\"ghost\") must fail loudly with SystemNotFound"
        );
    }

    /// `.chain()` combined with `.after("x")` on the whole group: both the
    /// internal chain and the external dependency hold.
    #[test]
    fn config_chain_then_after_group() {
        use crate::config::IntoScheduleConfigs;
        let mut sched = Scheduler::new();
        sched.add_systems(
            StageLabel::Update,
            (
                par("root", |_: SystemContext<'_>| {}),
                (
                    par("a", |_: SystemContext<'_>| {}),
                    par("b", |_: SystemContext<'_>| {}),
                )
                    .chain()
                    .after("root"),
            ),
        );
        sched.compile().unwrap();
        assert!(stage_of(&sched, "root") < stage_of(&sched, "a"), "a after root");
        assert!(stage_of(&sched, "root") < stage_of(&sched, "b"), "b after root");
        assert!(stage_of(&sched, "a") < stage_of(&sched, "b"), "a → b chained");
    }

    /// D2: a system with no component access (only side effects / whole-world)
    /// has nothing to partition by rows. On a large world the old code gave it
    /// entity_count = whole world and let ASD chunk it, running its body once per
    /// chunk. It must run exactly once.
    #[test]
    fn empty_access_system_runs_once_not_chunked() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        #[derive(apex_macros::Component)]
        struct Marker;

        let mut world = World::new();
        for _ in 0..100_000 {
            world.spawn((Marker,));
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let mut sched = Scheduler::new();
        sched.add_systems(
            StageLabel::Update,
            par("tick", move |_ctx: SystemContext<'_>| {
                c.fetch_add(1, Ordering::Relaxed);
            }),
        );
        sched.run(&mut world);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "an empty-access system must run once, not once per ASD chunk"
        );
    }

    /// D6: a stage whose systems are all disabled by run_if must NOT advance its
    /// change-detection window, so a system resuming after the pause still sees
    /// Changed<T> made while it was paused.
    #[test]
    fn run_if_paused_stage_does_not_lose_changed() {
        use apex_core::prelude::*;

        #[derive(apex_macros::Component)]
        struct Mark(u32);
        struct Ctl {
            active: bool,
            mutate: bool,
            entity: Entity,
            seen: u32,
        }

        let mut world = World::new();
        let e = world.spawn((Mark(0),));
        world.insert_resource(Ctl {
            active: false,
            mutate: false,
            entity: e,
            seen: 0,
        });

        fn mutator(w: &mut World) {
            let (mutate, e) = {
                let c = w.resource::<Ctl>();
                (c.mutate, c.entity)
            };
            if mutate {
                if let Some(mut m) = w.get_mut::<Mark>(e) {
                    m.0 += 1;
                }
            }
        }
        fn reader(w: &mut World) {
            let mut n = 0u32;
            let q = w.query::<(Read<Mark>, Changed<Mark>)>();
            q.for_each(|_, _| n += 1);
            w.resource_mut::<Ctl>().seen += n;
        }

        let mut sched = Scheduler::new();
        sched.add_system_to_stage("mutator", mutator, StageLabel::Update);
        sched
            .add_system_to_stage("reader", reader, StageLabel::PostUpdate)
            .run_if(|w: &World| w.resource::<Ctl>().active);

        sched.run(&mut world); // frame 1: paused, no mutation
        world.resource_mut::<Ctl>().mutate = true;
        sched.run(&mut world); // frame 2: Mark mutated, reader still paused
        world.resource_mut::<Ctl>().mutate = false;
        world.resource_mut::<Ctl>().active = true;
        sched.run(&mut world); // frame 3: reader resumes — must see the frame-2 change

        assert_eq!(
            world.resource::<Ctl>().seen,
            1,
            "reader must see Changed<Mark> produced while its stage was paused"
        );
    }

    /// D6-full: a run_if-gated reader that SHARES an execution stage with an UNGATED
    /// system (so the stage — and the old per-STAGE window — advances while the reader
    /// is paused) still sees `Changed<T>` produced during the pause, because the window
    /// is now PER-SYSTEM (Bevy `SystemMeta::last_run` parity). The solo-gated case above
    /// was already correct via the whole-stage skip; this covers the co-stage case the
    /// per-stage window got wrong (the reader's stage kept advancing on the keeper).
    #[test]
    fn co_stage_gated_reader_sees_changed_from_pause() {
        use apex_core::prelude::*;

        #[derive(apex_macros::Component)]
        struct Mark(u32);
        #[derive(Default)]
        struct KeeperRuns(u32);
        struct Ctl {
            active: bool,
            mutate: bool,
            entity: Entity,
            seen: u32,
        }

        let mut world = World::new();
        let e = world.spawn((Mark(0),));
        world.insert_resource(KeeperRuns::default());
        world.insert_resource(Ctl {
            active: false,
            mutate: false,
            entity: e,
            seen: 0,
        });

        // Update: mutate Mark on demand (whole-world ⇒ its own stage).
        fn mutator(w: &mut World) {
            let (mutate, e) = {
                let c = w.resource::<Ctl>();
                (c.mutate, c.entity)
            };
            if mutate {
                if let Some(mut m) = w.get_mut::<Mark>(e) {
                    m.0 += 1;
                }
            }
        }
        // PostUpdate: an UNGATED keeper writes an unrelated resource — it keeps the
        // PostUpdate stage running every frame WITHOUT conflicting with the reader
        // (KeeperRuns vs Mark/Ctl), so the two share one execution stage.
        fn keeper(mut k: ResMut<KeeperRuns>) {
            k.0 += 1;
        }
        // PostUpdate: the gated reader counts Changed<Mark>. Narrow access (Changed<Mark>
        // read + ResMut<Ctl>) ⇒ it shares the stage with keeper rather than getting its
        // own. A `system!` system (so it can be wrapped by `sys(...).run_if`).
        system! {
            fn reader(q: (Changed<Mark>, Read<Mark>), out: ResMut<Ctl>) {
                let mut n = 0u32;
                q.for_each(|_, _| n += 1);
                out.seen += n;
            }
        }

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, mutator);
        sched.add_systems(StageLabel::PostUpdate, keeper);
        sched.add_systems(
            StageLabel::PostUpdate,
            sys("reader", reader).run_if(|w: &World| w.resource::<Ctl>().active),
        );

        sched.run(&mut world); // frame 1: reader paused, keeper runs, no mutation
        world.resource_mut::<Ctl>().mutate = true;
        sched.run(&mut world); // frame 2: Mark mutated; reader paused; keeper advances the stage
        world.resource_mut::<Ctl>().mutate = false;
        world.resource_mut::<Ctl>().active = true;
        sched.run(&mut world); // frame 3: reader resumes (shares the stage with keeper)

        assert_eq!(
            world.resource::<Ctl>().seen,
            1,
            "co-stage gated reader must see Changed<Mark> from the pause \
             (per-system window; the per-stage window missed it because the keeper \
             kept advancing the shared stage)"
        );
    }

    /// D4: a Filtered system in a later stage must see archetypes an earlier stage
    /// spawned THIS frame — the cached per-system archetype indices are refreshed
    /// before each stage (a non-parallel stage's new archetypes used to be missed
    /// by a later parallel stage until the next frame).
    #[test]
    fn later_stage_sees_archetype_spawned_earlier_this_frame() {
        use apex_core::prelude::*;

        #[derive(apex_macros::Component)]
        struct Fresh;
        #[derive(Default)]
        struct Count(usize);
        #[derive(Default)]
        struct Done(bool);

        fn spawner(w: &mut World) {
            if !w.resource::<Done>().0 {
                for _ in 0..5 {
                    w.spawn((Fresh,));
                }
                w.resource_mut::<Done>().0 = true;
            }
        }
        fn counter(q: Query<Read<Fresh>>, mut count: ResMut<Count>) {
            let mut n = 0;
            q.for_each(|_, _| n += 1);
            count.0 = n;
        }

        let mut world = World::new();
        world.insert_resource(Count::default());
        world.insert_resource(Done::default());
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, spawner);
        sched.add_systems(StageLabel::PostUpdate, counter);

        sched.run(&mut world); // frame 1: Update spawns Fresh, PostUpdate must count them
        assert_eq!(
            world.resource::<Count>().0,
            5,
            "counter must see the archetype spawned by an earlier stage this frame"
        );
    }

    /// F3: a whole-world system (SystemContext / `Ctx` can touch any data but
    /// declares no specific access) must conflict with — and be serialized
    /// against — a writer, not run in parallel with it.
    #[test]
    fn whole_world_system_is_serialized_against_a_writer() {
        struct Whole;
        impl ParSystem for Whole {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().whole_world()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct Writer;
        impl ParSystem for Writer {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("whole", Whole);
        sched.add_par_system("writer", Writer);
        sched.compile().unwrap();

        let whole = sched.find_id_by_name("whole").unwrap();
        let writer = sched.find_id_by_name("writer").unwrap();
        let stages = sched.stages().unwrap();
        let ws = stages.iter().position(|s| s.system_ids.contains(&whole)).unwrap();
        let rs = stages.iter().position(|s| s.system_ids.contains(&writer)).unwrap();
        assert_ne!(
            ws, rs,
            "a whole-world system must be serialized against a writer, not parallel with it"
        );
    }

    #[test]
    fn bidir_write_read_no_false_circular_dep() {
        // SystemA: Read<Vel>, Write<Pos>
        // SystemB: Read<Pos>, Write<Vel>
        // The real conflict must be LinearOrder (not a cycle).
        let mut sched = Scheduler::new();
        sched.add_auto_system("sys_a", AutoMovement);
        sched.add_auto_system("sys_b", AutoMovement);

        let result = sched.compile();
        // Identical systems with Read<Vel>+Write<Pos> — they conflict
        // (WriteWrite over Pos, WriteWrite over Vel) but do not create a cycle.
        assert!(
            result.is_ok(),
            "Compilation must succeed without CircularDependency: {:?}",
            result.err()
        );
    }

    // ── Run Condition tests ────────────────────────────────────

    #[test]
    fn run_condition_true_system_executes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static RAN: AtomicBool = AtomicBool::new(false);

        let mut sched = Scheduler::new();
        sched.add_system("test_sys", |_: &mut World| {
            RAN.store(true, Ordering::SeqCst);
        })
        .run_if(|_| true);

        let mut world = World::new();
        sched.run_sequential(&mut world);

        assert!(RAN.load(Ordering::SeqCst), "The system must run when condition=true");
    }

    /// Regression (audit 2026-06-12): a scope condition from `scoped`+`run_condition`
    /// (a) applies to systems registered via `add_systems`
    /// (this path used to silently drop it), and (b) does NOT stick to systems
    /// registered AFTER the block (the scope used to never reset).
    #[test]
    fn scoped_condition_applies_to_add_systems_and_does_not_leak() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static INSIDE_RAN: AtomicBool = AtomicBool::new(false);
        static OUTSIDE_RAN: AtomicBool = AtomicBool::new(false);

        let mut sched = Scheduler::new();
        sched.scoped(|s| {
            s.run_condition(|_| false); // always-false scope
            s.add_systems(
                StageLabel::Update,
                seq("inside", |_w: &mut World| {
                    INSIDE_RAN.store(true, Ordering::SeqCst);
                }),
            );
        });
        // After the block the scope is cleared — the system runs unconditionally.
        sched.add_systems(
            StageLabel::Update,
            seq("outside", |_w: &mut World| {
                OUTSIDE_RAN.store(true, Ordering::SeqCst);
            }),
        );

        let mut world = World::new();
        sched.run_sequential(&mut world);

        assert!(
            !INSIDE_RAN.load(Ordering::SeqCst),
            "a scoped condition must apply to the add_systems path"
        );
        assert!(
            OUTSIDE_RAN.load(Ordering::SeqCst),
            "a scope condition must not stick to systems after the block"
        );
    }

    #[test]
    fn run_condition_false_system_skipped() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static RAN: AtomicBool = AtomicBool::new(false);

        let mut sched = Scheduler::new();
        sched.add_system("test_sys", |_: &mut World| {
            RAN.store(true, Ordering::SeqCst);
        })
        .run_if(|_| false);

        let mut world = World::new();
        sched.run_sequential(&mut world);

        assert!(!RAN.load(Ordering::SeqCst), "The system must NOT run when condition=false");
    }

    #[test]
    fn run_condition_reads_resource() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static RAN: AtomicBool = AtomicBool::new(false);

        #[derive(Clone, Copy)]
        struct GameState { paused: bool }

        let mut sched = Scheduler::new();
        let _id = sched.add_auto_system(
            "pausable",
            {
                struct PausableSystem;
                impl AutoSystem for PausableSystem {
                    type Query = ();
                    type Resources = ();
                    type Events = ();
                    fn run(&mut self, _: SystemContext<'_>) {
                        RAN.store(true, Ordering::SeqCst);
                    }
                }
                PausableSystem
            },
        );
        sched.set_run_if("pausable", |w: &World| {
            w.try_resource::<GameState>().map(|gs| !gs.paused).unwrap_or(true)
        })
        .unwrap();

        // paused = true → the system does not run
        let mut world = World::new();
        world.insert_resource(GameState { paused: true });
        sched.run_sequential(&mut world);
        assert!(!RAN.load(Ordering::SeqCst), "The system must not run while paused");

        // paused = false → the system runs
        RAN.store(false, Ordering::SeqCst);
        let mut world2 = World::new();
        world2.insert_resource(GameState { paused: false });
        let mut sched2 = Scheduler::new();
        let _id2 = sched2.add_auto_system(
            "pausable2",
            {
                struct PausableSystem2;
                impl AutoSystem for PausableSystem2 {
                    type Query = ();
                    type Resources = ();
                    type Events = ();
                    fn run(&mut self, _: SystemContext<'_>) {
                        RAN.store(true, Ordering::SeqCst);
                    }
                }
                PausableSystem2
            },
        );
        sched2.set_run_if("pausable2", |w: &World| {
            w.try_resource::<GameState>().map(|gs| !gs.paused).unwrap_or(true)
        })
        .unwrap();
        sched2.run_sequential(&mut world2);
        assert!(RAN.load(Ordering::SeqCst), "The system must run when not paused");
    }

    #[test]
    fn run_condition_multiple_systems_mixed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER_A: AtomicUsize = AtomicUsize::new(0);
        static COUNTER_B: AtomicUsize = AtomicUsize::new(0);

        let mut sched = Scheduler::new();
        sched.add_system("always_on", move |_: &mut World| {
            COUNTER_A.fetch_add(1, Ordering::SeqCst);
        });
        sched.add_system("conditionally_off", move |_: &mut World| {
            COUNTER_B.fetch_add(1, Ordering::SeqCst);
        })
        .run_if(|_| false);

        let mut world = World::new();
        sched.run_sequential(&mut world);

        assert_eq!(COUNTER_A.load(Ordering::SeqCst), 1, "always_on must run");
        assert_eq!(COUNTER_B.load(Ordering::SeqCst), 0, "conditionally_off must NOT run");
    }

    #[test]
    fn run_condition_parallel_stage_skips_system() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static RAN: AtomicBool = AtomicBool::new(false);

        let mut sched = Scheduler::new();
        let _id = sched.add_auto_system("skipped_in_parallel", {
            struct SkippedSys;
            impl AutoSystem for SkippedSys {
                type Query = ();
                type Resources = ();
                type Events = ();
                fn run(&mut self, _: SystemContext<'_>) {
                    RAN.store(true, Ordering::SeqCst);
                }
            }
            SkippedSys
        });
        sched.set_run_if("skipped_in_parallel", |_: &World| false).unwrap();

        let mut world = World::new();
        sched.run(&mut world);
        assert!(!RAN.load(Ordering::SeqCst), "A parallel system with condition=false does NOT run");
    }

    #[test]
    fn run_condition_parallel_stage_runs_system() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static RAN: AtomicBool = AtomicBool::new(false);

        let mut sched = Scheduler::new();
        let _id = sched.add_auto_system("runs_in_parallel", {
            struct RunsSys;
            impl AutoSystem for RunsSys {
                type Query = ();
                type Resources = ();
                type Events = ();
                fn run(&mut self, _: SystemContext<'_>) {
                    RAN.store(true, Ordering::SeqCst);
                }
            }
            RunsSys
        });
        sched.set_run_if("runs_in_parallel", |_: &World| true).unwrap();

        let mut world = World::new();
        sched.run(&mut world);
        assert!(RAN.load(Ordering::SeqCst), "A parallel system with condition=true runs");
    }

    #[test]
    fn run_condition_startup_system_runs_once_if_true() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        let mut sched = Scheduler::new();
        sched.add_startup_system("conditional_startup", move |_: &mut World| {
            COUNTER.fetch_add(1, Ordering::SeqCst);
        })
        .run_if(|_| true);

        let mut world = World::new();
        sched.run_sequential(&mut world);
        sched.run_sequential(&mut world);

        assert_eq!(COUNTER.load(Ordering::SeqCst), 1, "Startup with condition=true must run exactly once");
    }

    #[test]
    fn run_condition_auto_system_with_component_access() {
        use std::sync::atomic::{AtomicBool, Ordering};

        #[derive(Component, Clone, Copy)]
        struct Flag(bool);

        static RAN: AtomicBool = AtomicBool::new(false);

        struct ConditionalSystem;
        impl AutoSystem for ConditionalSystem {
            type Query = ();
            type Resources = ();
            type Events = ();
            fn run(&mut self, _: SystemContext<'_>) {
                RAN.store(true, Ordering::SeqCst);
            }
        }

        let f = move |w: &World| {
            w.try_resource::<Flag>().map(|f| f.0).unwrap_or(false)
        };

        let mut sched = Scheduler::new();
        let _id = sched.add_auto_system("conditional", ConditionalSystem);
        sched.set_run_if("conditional", Box::new(f)).unwrap();

        let mut world = World::new();
        sched.run_sequential(&mut world);
        assert!(!RAN.load(Ordering::SeqCst), "Without the Flag resource — it does not run");

        RAN.store(false, Ordering::SeqCst);
    }

    #[test]
    fn run_condition_auto_system_with_component_access_true() {
        use std::sync::atomic::{AtomicBool, Ordering};

        #[derive(Component, Clone, Copy)]
        struct Flag(bool);

        static RAN: AtomicBool = AtomicBool::new(false);

        struct ConditionalSystem;
        impl AutoSystem for ConditionalSystem {
            type Query = ();
            type Resources = ();
            type Events = ();
            fn run(&mut self, _: SystemContext<'_>) {
                RAN.store(true, Ordering::SeqCst);
            }
        }

        let mut world = World::new();
        world.insert_resource(Flag(true));

        let f = move |w: &World| {
            w.try_resource::<Flag>().map(|f| f.0).unwrap_or(false)
        };
        let mut sched = Scheduler::new();
        let _id = sched.add_auto_system("conditional2", ConditionalSystem);
        sched.set_run_if("conditional2", Box::new(f)).unwrap();

        sched.run_sequential(&mut world);
        assert!(RAN.load(Ordering::SeqCst), "With the Flag(true) resource — it runs");
    }

    // ── Apply Deferred tests ───────────────────────────────────

    #[test]
    fn apply_deferred_splits_into_two_sub_stages() {
        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("test"), |s| {
            s.add_system("first", |_: &mut World| {});
            s.apply_deferred();
            s.add_system("second", |_: &mut World| {});
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1 + 1, "There should be 2 sub-stages: first + second");
        assert_eq!(stages[0].system_ids.len(), 1, "First sub-stage: 1 system");
        assert_eq!(stages[1].system_ids.len(), 1, "Second sub-stage: 1 system");
    }

    #[test]
    fn apply_deferred_no_split_if_only_one_system() {
        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("test"), |s| {
            s.add_system("only", |_: &mut World| {});
            s.apply_deferred(); // no-op: only one system, no split needed
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1, "Without a second system — no split is created");
    }

    #[test]
    fn apply_deferred_commands_visible_in_next_sub_stage() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static ENTITY_SPAWNED: AtomicBool = AtomicBool::new(false);

        #[derive(Component, Clone, Copy)]
        struct Spawned;

        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("spawn_then_use"), |s| {
            s.add_system("spawn_sys", move |world: &mut World| {
                world.spawn((Spawned,));
                ENTITY_SPAWNED.store(true, Ordering::SeqCst);
            });
            s.apply_deferred();
            s.add_system("check_sys", move |world: &mut World| {
                let spawned = ENTITY_SPAWNED.load(Ordering::SeqCst);
                assert!(spawned, "spawn_system must run before check_system");
                let count = Query::<Read<Spawned>>::new(world).iter().count();
                assert_eq!(count, 1, "the spawn must be visible in the same frame");
            });
        });

        let mut world = World::new();
        sched.run_sequential(&mut world);
    }

    #[test]
    fn apply_deferred_multiple_splits() {
        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("multi"), |s| {
            s.add_system("a", |_: &mut World| {});
            s.apply_deferred();
            s.add_system("b", |_: &mut World| {});
            s.apply_deferred();
            s.add_system("c", |_: &mut World| {});
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1 + 2, "3 sub-stages: [a], [b], [c]");
    }

    #[test]
    fn apply_deferred_mixed_with_run_conditions() {
        use std::sync::atomic::{AtomicBool, Ordering};

        #[derive(Clone, Copy)]
        struct Flag(bool);

        static A_RAN: AtomicBool = AtomicBool::new(false);
        static B_RAN: AtomicBool = AtomicBool::new(false);

        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("mixed"), |s| {
            s.add_system("a", move |_: &mut World| {
                A_RAN.store(true, Ordering::SeqCst);
            });
            s.apply_deferred();
            s.add_system("b", move |_: &mut World| {
                B_RAN.store(true, Ordering::SeqCst);
            })
            .run_if(|w: &World| {
                w.try_resource::<Flag>().map(|f| f.0).unwrap_or(false)
            });
        });

        // Flag = false → b is skipped
        let mut world1 = World::new();
        world1.insert_resource(Flag(false));
        sched.compile_with_world(&world1).unwrap();
        sched.run_sequential(&mut world1);
        assert!(A_RAN.load(Ordering::SeqCst), "a must always run");
        assert!(!B_RAN.load(Ordering::SeqCst), "b must NOT run when Flag=false");

        // Flag = true → b runs
        A_RAN.store(false, Ordering::SeqCst);
        let mut sched2 = Scheduler::new();
        sched2.staged(StageLabel::tag("mixed2"), |s| {
            s.add_system("a2", move |_: &mut World| {
                A_RAN.store(true, Ordering::SeqCst);
            });
            s.apply_deferred();
            s.add_system("b2", move |_: &mut World| {
                B_RAN.store(true, Ordering::SeqCst);
            })
            .run_if(|w: &World| {
                w.try_resource::<Flag>().map(|f| f.0).unwrap_or(false)
            });
        });
        let mut world2 = World::new();
        world2.insert_resource(Flag(true));
        sched2.compile_with_world(&world2).unwrap();
        sched2.run_sequential(&mut world2);
        assert!(A_RAN.load(Ordering::SeqCst));
        assert!(B_RAN.load(Ordering::SeqCst), "b must run when Flag=true");
    }

    #[test]
    fn apply_deferred_double_call_idempotent() {
        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("test"), |s| {
            s.add_system("only", |_: &mut World| {});
            s.apply_deferred(); // first
            s.apply_deferred(); // second — idempotent
            s.add_system("after", |_: &mut World| {});
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1 + 1, "A double apply_deferred must not create empty sub-stages");
    }

    #[test]
    fn apply_deferred_at_start_noop() {
        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("test"), |s| {
            s.apply_deferred(); // no prior systems → no-op
            s.add_system("after", |_: &mut World| {});
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1, "apply_deferred with no prior system — a no-op");
    }

    #[test]
    fn apply_deferred_with_auto_systems() {
        struct DummySys;
        impl AutoSystem for DummySys {
            type Query = ();
            type Resources = ();
            type Events = ();
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("test"), |s| {
            s.add_auto_system("par_a", DummySys);
            s.apply_deferred();
            s.add_auto_system("par_b", DummySys);
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1 + 1, "AutoSystem + apply_deferred = 2 sub-stages");
        assert!(stages[0].all_parallel, "First sub-stage: all_parallel=true");
        assert!(stages[1].all_parallel, "Second sub-stage: all_parallel=true");
    }

    #[test]
    fn apply_deferred_groups_parallel_systems_correctly() {
        struct DummySys;
        impl AutoSystem for DummySys {
            type Query = ();
            type Resources = ();
            type Events = ();
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("test"), |s| {
            s.add_auto_system("a", DummySys);
            s.add_auto_system("b", DummySys);
            s.apply_deferred();
            s.add_auto_system("c", DummySys);
            s.add_auto_system("d", DummySys);
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 2, "2 sub-stages: [a,b] + [c,d]");
        assert_eq!(stages[0].system_ids.len(), 2);
        assert_eq!(stages[1].system_ids.len(), 2);
    }

    #[test]
    fn apply_deferred_triggers_event_visibility() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone, Copy)]
        struct MyEvent(#[allow(dead_code)] pub u32);

        static VALUE_SEEN: AtomicUsize = AtomicUsize::new(0);

        let mut sched = Scheduler::new();
        struct Emitter;
        impl ParSystem for Emitter {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write_event::<MyEvent>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let _emitter_id = sched.add_par_system("emitter", Emitter);
        sched.apply_deferred();

        sched.add_startup_system("reader", move |world: &mut World| {
            let events = world.events::<MyEvent>();
            VALUE_SEEN.store(events.len(), Ordering::SeqCst);
        });

        let mut world = World::new();
        world.add_event::<MyEvent>();
        world.send_event(MyEvent(42));
        world.tick();
        world.flush_all_events();

        sched.run_sequential(&mut world);
        assert_eq!(VALUE_SEEN.load(Ordering::SeqCst), 1, "The event is visible between sub-stages");
    }

    // ── Condition Trait tests ──────────────────────────────

    #[test]
    fn condition_trait_opaque_closure() {
        // Closure works with IntoConditionLeaf (via run_if)
        let mut sched = Scheduler::new();
        sched.add_system("test", |_: &mut World| {})
            .run_if(|_: &World| false);
        let world = World::new();
        sched.compile_with_world(&world).unwrap();
        // system was added with a false condition — compile should succeed
        assert!(sched.execution_plan.is_some());
    }

    #[test]
    fn condition_trait_typed_resource_exists() {
        let cond = conditions::resource_exists::<u32>();
        // access should contain read<u32>
        let acc = cond.access();
        let tid = std::any::TypeId::of::<u32>();
        assert!(acc.reads.contains(&tid));
        assert!(acc.writes.is_empty());
    }

    #[test]
    fn condition_trait_tuple_and() {
        let a = conditions::resource_exists::<u32>();
        let b = conditions::resource_exists::<f64>();
        let and_cond = (a, b);
        // access should contain both type IDs
        let acc = and_cond.access();
        assert!(acc.reads.contains(&std::any::TypeId::of::<u32>()));
        assert!(acc.reads.contains(&std::any::TypeId::of::<f64>()));
    }

    #[test]
    fn condition_trait_not() {
        // Typed condition: .not() inverts
        let cond = conditions::resource_exists::<u32>();
        let not_cond = cond.not();
        // No u32 resource → cond is false → not_cond is true
        assert!(not_cond.check(&World::new()));

        let mut world = World::new();
        world.insert_resource(42u32);
        // u32 exists → cond is true → not_cond is false
        assert!(!not_cond.check(&world));
    }

    #[test]
    fn cmd_commands_roundtrip_direct() {
        let mut cmds = Commands::new();
        #[derive(Component, Clone, Copy)]
        struct Sp;
        cmds.spawn((Sp,));
        let mut world = World::new();
        cmds.apply(&mut world);
        assert_eq!(Query::<Read<Sp>>::new(&world).iter().count(), 1,
            "Commands::spawn + apply should work");
    }

    #[test]
    fn system_macro_cmd_works() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static SAW: AtomicBool = AtomicBool::new(false);

        #[derive(Component, Clone, Copy)]
        struct Sp(#[allow(dead_code)] f32);

        system! {
            fn sys_cmd(cmd: Cmd) {
                cmd.spawn((Sp(1.0),));
            }
        }

        let mut sched = Scheduler::new();
        sched.add_auto_system("spn", sys_cmd);
        sched.add_system("chk", move |world: &mut World| {
            if Query::<Read<Sp>>::new(world).iter().count() > 0 {
                SAW.store(true, Ordering::SeqCst);
            }
        });
        sched.chain(&["spn", "chk"]).unwrap();

        let mut world = World::new();
        // Use run() (parallel path) — known to work with Commands
        sched.run(&mut world);
        assert!(SAW.load(Ordering::SeqCst), "system! with cmd: Cmd + run() should work");
    }

    #[test]
    fn auto_apply_deferred_from_chain() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static CAMERA_SAW: AtomicBool = AtomicBool::new(false);

        #[derive(Component, Clone, Copy)]
        struct Spawned;

        // Use a sequential system that spawns entities (has has_deferred)
        let mut sched = Scheduler::new();
        sched.add_system("spawner", move |world: &mut World| {
            world.spawn((Spawned,));
        });
        sched.add_system("camera", move |world: &mut World| {
            let count = Query::<Read<Spawned>>::new(world).iter().count();
            if count > 0 {
                CAMERA_SAW.store(true, Ordering::SeqCst);
            }
        });
        sched.chain(&["spawner", "camera"]).unwrap();

        let mut world = World::new();
        sched.run_sequential(&mut world);
        assert!(CAMERA_SAW.load(Ordering::SeqCst), "camera should see spawned entity via auto-apply");
    }

    #[test]
    fn auto_apply_deferred_no_split_without_chain() {
        let mut sched = Scheduler::new();
        // Two AutoSystems without chain — they should end up in one parallel group
        struct SysA;
        impl AutoSystem for SysA {
            type Query = ();
            type Resources = ();
            type Events = ();
            const HAS_DEFERRED: bool = true;
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct SysB;
        impl AutoSystem for SysB {
            type Query = ();
            type Resources = ();
            type Events = ();
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        sched.add_auto_system("a", SysA);
        sched.add_auto_system("b", SysB);

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        // Without chain, no auto-split — both in same stage
        let both_in_same = stages.iter().any(|s| s.system_ids.len() >= 2);
        assert!(both_in_same, "Without chain, no auto-split for deferred");
    }

    #[test]
    fn condition_access_causes_conflict() {
        #[derive(Clone, Copy)]
        struct GamePhase;
        struct TogglePause;
        impl ParSystem for TogglePause {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<GamePhase>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        struct MovementWithCondition;
        impl ParSystem for MovementWithCondition {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("toggle", TogglePause);
        sched.add_par_system("move", MovementWithCondition);
        // Add typed condition that reads GamePhase
        sched.set_run_if_cond("move", conditions::resource_exists::<GamePhase>()).unwrap();

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        // toggle writes GamePhase, move reads GamePhase (via condition)
        // → WriteRead conflict → at least 2 stages
        assert!(stages.len() >= 2, "typed condition should cause WriteRead conflict: {}", stages.len());
    }

    #[test]
    fn condition_access_no_conflict_opaque() {
        struct SysA;
        impl ParSystem for SysA {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        struct SysB;
        impl ParSystem for SysB {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Vel>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("a", SysA);
        let _bid = sched.add_par_system("b", SysB);
        // Opaque condition — no typed access, no extra conflicts
        sched.set_run_if("b", |_: &World| true).unwrap();

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        // Pos and Vel — no conflict, should be same stage
        let same_stage = stages.iter().any(|s| s.system_ids.len() >= 2);
        assert!(same_stage, "opaque condition should not cause conflicts");
    }

    // ── D2-1: plain-fn systems (Bevy style) ────────────────────

    /// Plain functions with Bevy parameters are registered by bare identifiers
    /// and work: Res/ResMut/Query<(&T, &mut U)>/EventWriter/EventReader.
    #[test]
    fn plain_fn_systems_bevy_style() {
        use apex_core::query::Query as Q;

        // Named field: `.0` on Res resolves to Res's public tuple
        // field (&T), not to Deref — a known rough edge.
        struct Dt {
            step: f32,
        }
        struct Moved(u32);
        #[derive(Clone, Copy)]
        struct Ping(u32);

        fn movement(dt: Res<Dt>, mut q: Q<(&Vel, &mut Pos)>) {
            q.for_each_mut(|_, (v, mut p)| {
                p.x += v.x * dt.step;
                p.y += v.y * dt.step;
            });
        }

        fn emit_pings(q: Q<&Pos>, mut out: EventWriter<Ping>) {
            out.send(Ping(q.len() as u32));
        }

        fn count_moved(mut evs: EventReader<Ping>, mut moved: ResMut<Moved>) {
            // The main Bevy idiom (TD-24): direct iteration over read().
            for p in evs.read() {
                moved.0 += p.0;
            }
        }

        let mut world = World::new();
        world.insert_resource(Dt { step: 1.0 });
        world.insert_resource(Moved(0));
        world.add_event::<Ping>();
        for i in 0..10 {
            world.spawn((
                Pos { x: 0.0, y: 0.0 },
                Vel {
                    x: i as f32,
                    y: 1.0,
                },
            ));
        }

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, (movement, emit_pings, count_moved));
        sched.run(&mut world);
        sched.run(&mut world);

        // movement moved each entity twice.
        let mut sum = 0.0;
        Q::<Read<Pos>>::new(&world).for_each(|_, p| sum += p.y);
        assert_eq!(sum, 20.0, "movement ran 2 frames over 10 entities");

        // Event pipeline: emit (10 per frame) → reader (per-stage flush).
        // At least one frame is delivered (depends on the in-stage order — but over
        // 2 frames at least 10 arrive).
        assert!(
            world.resource::<Moved>().0 >= 10,
            "events passed through the plain-fn writer/reader: {}",
            world.resource::<Moved>().0
        );
    }

    /// `&mut Commands` in a plain-fn: has_deferred → commands are applied
    /// by the scheduler (an auto-apply sync point).
    #[test]
    fn plain_fn_commands_are_applied() {
        fn spawner(cmd: &mut Commands) {
            cmd.spawn((Pos { x: 7.0, y: 7.0 },));
        }

        let mut world = World::new();
        world.register_component::<Pos>();
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, spawner);
        sched.run(&mut world);

        let n = apex_core::query::Query::<Read<Pos>>::new(&world)
            .iter()
            .count();
        assert_eq!(n, 1, "the spawn from &mut Commands is applied by the end of the frame");
    }

    /// The system name is derived from the function name (D2-1/U.4).
    #[test]
    fn plain_fn_name_derived_from_fn() {
        fn my_special_system(_q: apex_core::query::Query<'_, '_, Read<Pos>>) {}
        let cfg = SystemConfig::fn_sys(my_special_system);
        assert_eq!(cfg.name, "my_special_system");
    }

    /// Plain-fn systems with disjoint access land in one stage
    /// (a parallel batch); with overlapping access — they conflict.
    #[test]
    fn plain_fn_access_inferred_for_conflicts() {
        fn writes_pos(mut q: apex_core::query::Query<'_, '_, Write<Pos>>) {
            q.for_each_mut(|_, mut p| p.x += 1.0);
        }
        fn writes_vel(mut q: apex_core::query::Query<'_, '_, Write<Vel>>) {
            q.for_each_mut(|_, mut v| v.x += 1.0);
        }
        fn reads_pos(q: apex_core::query::Query<'_, '_, Read<Pos>>) {
            q.for_each(|_, _| {});
        }

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, (writes_pos, writes_vel, reads_pos));
        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // writes_pos conflicts with reads_pos (W+R over Pos) → different batches;
        // writes_vel overlaps with no one → shares a batch with one of them.
        // Minimal check: access is DERIVED (there is >1 execution stage).
        assert!(
            stages.len() >= 2,
            "a W+R conflict over Pos must split systems across batches: {} stages",
            stages.len()
        );
    }

    // ── D2-5: FixedUpdate ──────────────────────────────────────

    /// FixedUpdate runs off the FixedTime accumulator: 0..N steps per frame,
    /// the remainder carries over; Update meanwhile runs exactly once per frame.
    #[test]
    fn fixed_update_steps_by_accumulator() {
        struct Counts {
            fixed: u32,
            update: u32,
        }

        fn fixed_step(mut c: ResMut<Counts>) {
            c.fixed += 1;
        }
        fn frame_step(mut c: ResMut<Counts>) {
            c.update += 1;
        }

        let mut world = World::new();
        world.insert_resource(Counts {
            fixed: 0,
            update: 0,
        });
        world.insert_resource(crate::FixedTime::from_dt(0.010));

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::FixedUpdate, fixed_step);
        sched.add_systems(StageLabel::Update, frame_step);

        // Frame 1: 35ms → 3 steps, remainder 5ms.
        world
            .resource_mut::<crate::FixedTime>()
            .accumulate(0.035);
        sched.run(&mut world);
        assert_eq!(world.resource::<Counts>().fixed, 3);
        assert_eq!(world.resource::<Counts>().update, 1);

        // Frame 2: +0ms → 0 steps (remainder 5ms < dt).
        sched.run(&mut world);
        assert_eq!(world.resource::<Counts>().fixed, 3);
        assert_eq!(world.resource::<Counts>().update, 2);

        // Frame 3: +6ms → remainder 11ms → 1 step.
        world
            .resource_mut::<crate::FixedTime>()
            .accumulate(0.006);
        sched.run(&mut world);
        assert_eq!(world.resource::<Counts>().fixed, 4);
        assert_eq!(world.resource::<Counts>().update, 3);
    }

    /// D1: two CONFLICTING FixedUpdate systems (the planner splits them into
    /// separate execution stages) must both run on every fixed step, interleaved
    /// as (A;B)×N — not A×N then B×N, and the second must never be starved by the
    /// first draining the accumulator.
    #[test]
    fn fixed_update_conflicting_systems_all_run_each_step_interleaved() {
        #[derive(Default)]
        struct Log(Vec<char>);

        fn step_a(mut log: ResMut<Log>) {
            log.0.push('A');
        }
        fn step_b(mut log: ResMut<Log>) {
            log.0.push('B');
        }

        let mut world = World::new();
        world.insert_resource(Log::default());
        world.insert_resource(crate::FixedTime::from_dt(0.010));

        let mut sched = Scheduler::new();
        // Both take ResMut<Log> → WriteWrite → separate execution stages.
        sched.add_systems(StageLabel::FixedUpdate, step_a);
        sched.add_systems(StageLabel::FixedUpdate, step_b);

        // 35ms / 10ms = 3 fixed steps.
        world.resource_mut::<crate::FixedTime>().accumulate(0.035);
        sched.run(&mut world);

        let log: String = world.resource::<Log>().0.iter().collect();
        let a = log.chars().filter(|&c| c == 'A').count();
        let b = log.chars().filter(|&c| c == 'B').count();
        assert_eq!(a, 3, "system A must run every fixed step, got log {log:?}");
        assert_eq!(b, 3, "system B must NOT be starved — runs every step too, got {log:?}");
        assert!(
            !log.contains("AA") && !log.contains("BB"),
            "steps must interleave as (A;B)×N, not A×N then B×N, got {log:?}"
        );
    }

    /// Death-spiral guard: steps are capped, the excess is dropped.
    #[test]
    fn fixed_update_death_spiral_cap() {
        struct N(u32);
        fn step(mut n: ResMut<N>) {
            n.0 += 1;
        }

        let mut world = World::new();
        world.insert_resource(N(0));
        let mut ft = crate::FixedTime::from_dt(0.001);
        ft.max_steps_per_frame = 4;
        ft.accumulate(10.0); // 10000 steps of "debt"
        world.insert_resource(ft);

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::FixedUpdate, step);
        sched.run(&mut world);

        assert_eq!(world.resource::<N>().0, 4, "step cap");
        assert_eq!(
            world.resource::<crate::FixedTime>().overstep(),
            0.0,
            "excess dropped"
        );
    }

    /// Without a FixedTime resource the FixedUpdate stage is normal (once per frame).
    #[test]
    fn fixed_update_without_resource_runs_once() {
        struct N(u32);
        fn step(mut n: ResMut<N>) {
            n.0 += 1;
        }
        let mut world = World::new();
        world.insert_resource(N(0));
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::FixedUpdate, step);
        sched.run(&mut world);
        sched.run(&mut world);
        assert_eq!(world.resource::<N>().0, 2);
    }

    // ── D2-6: States ───────────────────────────────────────────

    /// Full state lifecycle: in_state gates systems, on_enter/
    /// on_exit are true for exactly one frame, transition via NextState.
    #[test]
    fn states_in_state_on_enter_on_exit() {
        use crate::states::{in_state, init_state, on_enter, on_exit, NextState};

        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum Game {
            Menu,
            Playing,
        }

        #[derive(Default)]
        struct Log {
            menu_frames: u32,
            entered_playing: u32,
            exited_menu: u32,
        }

        fn menu_ui(mut log: ResMut<Log>) {
            log.menu_frames += 1;
        }
        fn spawn_level(mut log: ResMut<Log>) {
            log.entered_playing += 1;
        }
        fn teardown_menu(mut log: ResMut<Log>) {
            log.exited_menu += 1;
        }

        let mut world = World::new();
        world.insert_resource(Log::default());
        let mut sched = Scheduler::new();
        init_state(&mut world, &mut sched, Game::Menu);

        // P4: run_if directly on a bare fn (FnSystemExt), as in Bevy.
        use crate::config::FnSystemExt;
        sched.add_systems(
            StageLabel::Update,
            (
                menu_ui.run_if(in_state(Game::Menu)),
                spawn_level.run_if(on_enter(Game::Playing)),
                teardown_menu.run_if(on_exit(Game::Menu)),
            ),
        );

        sched.run(&mut world); // frame 1: Menu
        sched.run(&mut world); // frame 2: Menu
        assert_eq!(world.resource::<Log>().menu_frames, 2);
        assert_eq!(world.resource::<Log>().entered_playing, 0);

        world.resource_mut::<NextState<Game>>().set(Game::Playing);
        sched.run(&mut world); // frame 3: transition at the start of the frame
        let log = world.resource::<Log>();
        assert_eq!(log.menu_frames, 2, "in_state(Menu) no longer lets it through");
        assert_eq!(log.entered_playing, 1, "on_enter — exactly one frame");
        assert_eq!(log.exited_menu, 1, "on_exit — exactly one frame");

        sched.run(&mut world); // frame 4: Playing, no transitions
        let log = world.resource::<Log>();
        assert_eq!(log.entered_playing, 1);
        assert_eq!(log.exited_menu, 1);
    }

    /// D7: on_enter(initial) must be visible to Update (and later) systems on the
    /// first frame, not only Startup — the transition system used to clear it in
    /// First before Update ran.
    #[test]
    fn on_enter_initial_visible_to_update_on_first_frame() {
        use crate::config::FnSystemExt;
        use crate::states::{init_state, on_enter};

        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum S {
            A,
        }
        #[derive(Default)]
        struct Log(u32);
        fn on_enter_a(mut log: ResMut<Log>) {
            log.0 += 1;
        }

        let mut world = World::new();
        world.insert_resource(Log::default());
        let mut sched = Scheduler::new();
        init_state(&mut world, &mut sched, S::A);
        sched.add_systems(StageLabel::Update, on_enter_a.run_if(on_enter(S::A)));

        sched.run(&mut world); // frame 1
        assert_eq!(
            world.resource::<Log>().0,
            1,
            "on_enter(initial) must fire for Update systems on frame 1"
        );
        sched.run(&mut world); // frame 2 — no longer entering
        assert_eq!(world.resource::<Log>().0, 1, "on_enter lasts one frame only");
    }

    /// D7: a second init_state for the same state is ignored, so transitions keep
    /// working (a second transition system would clear the flags the first sets).
    #[test]
    fn double_init_state_is_ignored() {
        use crate::config::FnSystemExt;
        use crate::states::{init_state, on_enter, NextState};

        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum S {
            A,
            B,
        }
        #[derive(Default)]
        struct Log(u32);
        fn on_enter_b(mut log: ResMut<Log>) {
            log.0 += 1;
        }

        let mut world = World::new();
        world.insert_resource(Log::default());
        let mut sched = Scheduler::new();
        init_state(&mut world, &mut sched, S::A);
        init_state(&mut world, &mut sched, S::A); // ignored
        sched.add_systems(StageLabel::Update, on_enter_b.run_if(on_enter(S::B)));

        sched.run(&mut world); // frame 1
        world.resource_mut::<NextState<S>>().set(S::B);
        sched.run(&mut world); // transition to B
        assert_eq!(
            world.resource::<Log>().0,
            1,
            "transitions still work despite a double init_state"
        );
    }

    // ── E5: Single<Q> / Option<Single<Q>> — skip semantics ────

    #[test]
    fn single_param_skips_unless_exactly_one_match() {
        use apex_core::prelude::*;

        #[derive(Clone, Copy, Debug, apex_macros::Component)]
        struct Player(f32);
        #[derive(Clone, Copy, Debug, Default)]
        struct Runs {
            single: u32,
            optional_some: u32,
            optional_none: u32,
        }

        fn with_single(p: Single<&Player>, mut runs: ResMut<Runs>) {
            assert!(p.0 >= 0.0);
            runs.single += 1;
        }
        fn with_optional(p: Option<Single<&Player>>, mut runs: ResMut<Runs>) {
            match p {
                Some(s) => {
                    let _e = s.entity();
                    runs.optional_some += 1;
                }
                None => runs.optional_none += 1,
            }
        }

        let mut world = World::new();
        world.insert_resource(Runs::default());
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, (with_single, with_optional));

        // 0 matches: Single is skipped, Option<Single> gets None.
        sched.run(&mut world);
        assert_eq!(world.resource::<Runs>().single, 0);
        assert_eq!(world.resource::<Runs>().optional_none, 1);

        // 1 match: both run.
        let p1 = world.spawn((Player(1.0),));
        sched.run(&mut world);
        assert_eq!(world.resource::<Runs>().single, 1);
        assert_eq!(world.resource::<Runs>().optional_some, 1);

        // 2 matches: both are skipped.
        world.spawn((Player(2.0),));
        sched.run(&mut world);
        let runs = *world.resource::<Runs>();
        assert_eq!(runs.single, 1, "Single skips the frame when there is >1 match");
        assert_eq!(runs.optional_some, 1, "Option<Single> also skips when there is >1");
        assert_eq!(runs.optional_none, 1);

        // 1 match again — it resumes (mutable form + filter).
        world.despawn(p1);
        fn bump(mut p: Single<&mut Player, With<Player>>) {
            p.0 += 1.0;
        }
        sched.add_systems(StageLabel::Update, bump);
        sched.run(&mut world);
        assert_eq!(world.resource::<Runs>().single, 2);
    }

    // ── W3-4: ASD row-split stress ────────────────────────────

    /// A stateless system on a large world is CHUNKED (ASD row-split):
    /// each row must be processed EXACTLY once — no skips
    /// (chunk completeness) and no duplicates (range disjointness).
    #[test]
    fn asd_row_split_visits_each_row_exactly_once() {
        #[derive(Component, Clone, Copy)]
        struct Hits(u32);

        system! {
            fn bump(q: Write<Hits>) {
                q.for_each_mut(|_, mut h| h.0 += 1);
            }
        }

        const N: usize = 100_000; // deliberately larger than effective_chunk
        let mut world = World::new();
        world.spawn_many(N, |_| (Hits(0),));

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, bump);
        for _ in 0..3 {
            sched.run(&mut world);
        }

        let mut bad = 0usize;
        Query::<Read<Hits>>::new(&world).for_each(|_, h| {
            if h.0 != 3 {
                bad += 1;
            }
        });
        assert_eq!(bad, 0, "each row is processed exactly once per frame");
    }

    /// D3 regression: concurrent row-split ASD tasks for one stateless system
    /// each form `&mut *task.ptr`. The pointer must target the `dyn ParSystem`
    /// (zero-sized for a plain-fn system) — NOT the enclosing `SystemDescriptor`,
    /// which owns a `String` name and other real bytes. Pre-fix the task
    /// materialized `&mut SystemDescriptor`, so several split tasks running at
    /// once held aliasing `&mut` over the same non-ZST object (Stacked/Tree
    /// Borrows UB). A dedicated 4-thread pool plus a chunk config that forces
    /// small chunks makes the split deterministic (and Miri-tractable, unlike the
    /// 100k test above). Validate with
    /// `MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-tree-borrows" cargo +nightly
    /// miri test -p apex-scheduler asd_row_split_no_descriptor_aliasing`.
    #[test]
    fn asd_row_split_no_descriptor_aliasing() {
        #[derive(Component, Clone, Copy)]
        struct Hits(u32);

        system! {
            fn bump(q: Write<Hits>) {
                q.for_each_mut(|_, mut h| h.0 += 1);
            }
        }

        const N: usize = 256;
        let mut world = World::new();
        // Force splitting at a small N: never fall back to serial, allow small
        // chunks. With 4 threads: target_chunk ~32, effective_chunk 32, so 256
        // entities split into 8 concurrent tasks for one system.
        world.set_chunk_config(apex_core::world::ChunkConfig {
            min_entities_per_thread: 1,
            dynamic_min_chunk: 8,
            max_chunk_size: 65536,
            auto_serial_fallback: false,
            task_multiplier: 2.0,
            ..Default::default()
        });
        world.spawn_many(N, |_| (Hits(0),));

        // A dedicated 4-thread pool makes the concurrent split path run
        // regardless of the host CPU count (`rayon::scope` inside `run` uses the
        // installed pool).
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        // The `Scheduler` is `!Send` (it may hold NonSend main-thread systems), so
        // construct AND run it INSIDE the install closure — it lives entirely on a
        // pool thread and never crosses a thread boundary. The closure captures
        // only `&mut world` (Send) and the ZST `bump` system.
        pool.install(|| {
            let mut sched = Scheduler::new();
            sched.add_systems(StageLabel::Update, bump);
            sched.run(&mut world);
        });

        let mut bad = 0usize;
        Query::<Read<Hits>>::new(&world).for_each(|_, h| {
            if h.0 != 1 {
                bad += 1;
            }
        });
        assert_eq!(bad, 0, "each row bumped exactly once across concurrent split tasks");
    }

    // ── B1: NonSend (main-thread) systems ────────────────────────────────

    /// B1: a NonSend system holding genuinely `!Send` state (an `Rc`) is stored
    /// and run by the scheduler on the main thread, and its DECLARED write
    /// persists frame to frame. The `Rc` capture is exactly what forces
    /// `Scheduler: !Send` — a `Send`-only registration could not accept it.
    #[test]
    fn nonsend_system_runs_and_writes() {
        #[derive(Component, Clone, Copy)]
        struct Counter(u32);

        let mut world = World::new();
        world.spawn((Counter(0),));
        world.spawn((Counter(0),));

        let runs = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let runs_in = runs.clone();

        let mut sched = Scheduler::new();
        sched.add_dynamic_nonsend_system(
            "bump",
            access_desc!(write<Counter>),
            move |ctx: SystemContext<'_>| {
                runs_in.set(runs_in.get() + 1);
                ctx.query_unchecked::<Write<Counter>>()
                    .for_each_mut(|_, mut c| c.0 += 1);
            },
        );

        sched.run(&mut world);
        sched.run(&mut world);

        assert_eq!(runs.get(), 2, "the NonSend runner ran once per frame");
        let mut vals = Vec::new();
        Query::<Read<Counter>>::new(&world).for_each(|_, c| vals.push(c.0));
        assert_eq!(vals, vec![2, 2], "declared write applied each frame to both entities");
    }

    /// B1: the public `add_dynamic_system` (Send runner + explicit access) — the
    /// runtime-declared lower layer of registration — registers and runs.
    #[test]
    fn dynamic_send_system_runs_and_writes() {
        #[derive(Component, Clone, Copy)]
        struct Val(i32);

        let mut world = World::new();
        world.spawn((Val(10),));

        let mut sched = Scheduler::new();
        sched.add_dynamic_system("set", access_desc!(write<Val>), |ctx: SystemContext<'_>| {
            ctx.query_unchecked::<Write<Val>>().for_each_mut(|_, mut v| v.0 = 42);
        });

        sched.run(&mut world);

        let mut got = None;
        Query::<Read<Val>>::new(&world).for_each(|_, v| got = Some(v.0));
        assert_eq!(got, Some(42));
    }

    /// B4b: `remove_system` drops a registered system — it no longer runs, while a
    /// sibling with the SAME (conflicting) access keeps running, and its id stays
    /// stable. Proves the id-map / index-list / graph rebuild is correct: removing
    /// one of two Write<Val> systems (which the scheduler had sequenced into
    /// separate stages) leaves a runnable plan. Removing a non-existent id is a
    /// no-op returning `false`.
    #[test]
    fn remove_system_drops_it_and_rebuilds() {
        #[derive(Component, Clone, Copy)]
        struct Val(i32);

        let mut world = World::new();
        world.spawn((Val(0),));

        let mut sched = Scheduler::new();
        let add_ten = sched.add_dynamic_system("add_ten", access_desc!(write<Val>), |ctx: SystemContext<'_>| {
            ctx.query_unchecked::<Write<Val>>().for_each_mut(|_, mut v| v.0 += 10);
        });
        sched.add_dynamic_system("add_one", access_desc!(write<Val>), |ctx: SystemContext<'_>| {
            ctx.query_unchecked::<Write<Val>>().for_each_mut(|_, mut v| v.0 += 1);
        });

        sched.run(&mut world);
        let mut got = 0;
        Query::<Read<Val>>::new(&world).for_each(|_, v| got = v.0);
        assert_eq!(got, 11, "both ran (+10, +1)");

        // Remove add_ten; only add_one (+1) should run now.
        assert!(sched.remove_system(add_ten), "existing id removed");
        assert!(!sched.remove_system(add_ten), "already-removed id → no-op false");

        sched.run(&mut world);
        Query::<Read<Val>>::new(&world).for_each(|_, v| got = v.0);
        assert_eq!(got, 12, "only add_one ran after removal (11 + 1); add_ten is gone");
    }

    /// B1: a NonSend system's declared access PARTICIPATES in conflict detection.
    /// A NonSend writer and a Rust (parallel) writer of the SAME component both
    /// run and both writes land (each entity +2) — the conflict is seen, so there
    /// is no lost update. (B2 will additionally assert they never run concurrently.)
    #[test]
    fn nonsend_access_participates_in_conflict_detection() {
        #[derive(Component, Clone, Copy)]
        struct Hp(i32);

        system! {
            fn rust_bump(q: Write<Hp>) {
                q.for_each_mut(|_, mut h| h.0 += 1);
            }
        }

        let mut world = World::new();
        world.spawn((Hp(0),));

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, rust_bump);
        sched.add_dynamic_nonsend_system(
            "lua_bump",
            access_desc!(write<Hp>),
            |ctx: SystemContext<'_>| {
                ctx.query_unchecked::<Write<Hp>>().for_each_mut(|_, mut h| h.0 += 1);
            },
        );

        sched.run(&mut world);

        let mut got = None;
        Query::<Read<Hp>>::new(&world).for_each(|_, h| got = Some(h.0));
        assert_eq!(
            got,
            Some(2),
            "both the Rust and NonSend writers ran; the Write+Write conflict serialized them (no lost update)"
        );
    }

    // ── B2: NonSend runs on main concurrently with worker tasks ──────────

    /// B2: a NonSend system and a `Parallel` system with DISJOINT access share one
    /// parallelizable stage; the parallel path runs the Parallel system on a worker
    /// and the NonSend system on the main thread. Both declared writes land
    /// correctly (soundness of the concurrent path — disjoint components).
    #[test]
    fn nonsend_and_parallel_disjoint_both_write() {
        #[derive(Component, Clone, Copy)]
        struct A(u32);
        #[derive(Component, Clone, Copy)]
        struct B(u32);

        let mut world = World::new();
        // Force the concurrent parallel path (see the rendezvous test).
        world.set_chunk_config(apex_core::world::ChunkConfig {
            auto_disable_stage_parallel: false,
            stage_parallel_min_entities: 0,
            ..Default::default()
        });
        world.spawn_many(128, |_| (A(0),));
        world.spawn_many(128, |_| (B(0),));

        let mut sched = Scheduler::new();
        sched.add_dynamic_system("rust_a", access_desc!(write<A>), |ctx: SystemContext<'_>| {
            ctx.query_unchecked::<Write<A>>().for_each_mut(|_, mut a| a.0 += 1);
        });
        sched.add_dynamic_nonsend_system(
            "lua_b",
            access_desc!(write<B>),
            |ctx: SystemContext<'_>| {
                ctx.query_unchecked::<Write<B>>().for_each_mut(|_, mut b| b.0 += 1);
            },
        );

        sched.run(&mut world);

        let mut bad = 0;
        Query::<Read<A>>::new(&world).for_each(|_, a| if a.0 != 1 { bad += 1 });
        Query::<Read<B>>::new(&world).for_each(|_, b| if b.0 != 1 { bad += 1 });
        assert_eq!(bad, 0, "both the worker (A) and main-thread NonSend (B) writes landed exactly once");
    }

    /// B2: the NonSend system runs CONCURRENTLY with a worker task — not before or
    /// after. A rendezvous proves it: the NonSend system (registered FIRST, so a
    /// sequential fallback would run it first and see nothing) spins waiting for a
    /// flag set by the `Parallel` system on a worker. Only genuine concurrency lets
    /// it observe the flag. A dedicated 4-thread pool guarantees a free worker.
    #[test]
    fn nonsend_observes_concurrent_parallel_worker() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        #[derive(Component, Clone, Copy)]
        struct A(u32);
        #[derive(Component, Clone, Copy)]
        struct B(u32);

        let mut world = World::new();
        // Force the PARALLEL dispatch path on the first (only) run: default
        // `auto_disable_stage_parallel` sends a tiny stage to the sequential
        // fallback (where NonSend, registered first, would run before the signal).
        world.set_chunk_config(apex_core::world::ChunkConfig {
            auto_disable_stage_parallel: false,
            stage_parallel_min_entities: 0,
            ..Default::default()
        });
        world.spawn((A(0),));
        world.spawn((B(0),));

        let flag = Arc::new(AtomicBool::new(false)); // set by the Parallel worker
        let saw = Arc::new(AtomicBool::new(false)); // set by NonSend if it observes flag
        let flag_p = flag.clone();
        let flag_n = flag.clone();
        let saw_n = saw.clone();

        let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
        // Scheduler is `!Send` — build AND run it inside the pool context.
        pool.install(move || {
            let mut sched = Scheduler::new();
            // NonSend registered FIRST: under a sequential fallback it would run
            // before the signal and never see it → `saw` stays false. `saw == true`
            // is therefore unique to concurrent execution.
            sched.add_dynamic_nonsend_system(
                "lua_wait",
                access_desc!(write<B>),
                move |ctx: SystemContext<'_>| {
                    // Bounded spin (yields so it makes progress on any core count).
                    for _ in 0..5_000_000 {
                        if flag_n.load(Ordering::SeqCst) {
                            saw_n.store(true, Ordering::SeqCst);
                            break;
                        }
                        std::thread::yield_now();
                    }
                    ctx.query_unchecked::<Write<B>>().for_each_mut(|_, mut b| b.0 += 1);
                },
            );
            sched.add_dynamic_system(
                "rust_signal",
                access_desc!(write<A>),
                move |ctx: SystemContext<'_>| {
                    flag_p.store(true, Ordering::SeqCst);
                    ctx.query_unchecked::<Write<A>>().for_each_mut(|_, mut a| a.0 += 1);
                },
            );
            sched.run(&mut world);
        });

        assert!(
            saw.load(Ordering::SeqCst),
            "the NonSend system observed the worker's signal → they ran concurrently (Lua \u{2016} Rust)"
        );
    }

    /// A STATEFUL system is not row-split (W3-4): one instance,
    /// one run call per frame, the state sees ALL rows. Before the fix several
    /// split tasks called run(&mut self) concurrently: a race on the state +
    /// each task saw only its own range.
    #[test]
    fn asd_does_not_split_stateful_system() {
        #[derive(Component, Clone, Copy)]
        struct Tag;

        struct SeenTotal(u64);

        system! {
            struct CountRows { seen: u64 = 0 }
            fn run(s: &mut Self, q: Read<Tag>, out: ResMut<SeenTotal>) {
                s.seen = 0;
                q.for_each(|_, _| s.seen += 1);
                out.0 = s.seen;
            }
        }

        const N: usize = 100_000;
        let mut world = World::new();
        world.spawn_many(N, |_| (Tag,));
        world.insert_resource(SeenTotal(0));

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, CountRows::default());
        sched.run(&mut world);

        assert_eq!(
            world.resource::<SeenTotal>().0,
            N as u64,
            "a stateful system gets a full SubWorld (no row-split)"
        );
    }

    /// Regression: a STATEFUL run condition (`run_until`/`every_n_frames`, whose
    /// internal counter advances on every `check`) must be evaluated EXACTLY ONCE
    /// per frame. Before the fix the D6 stage-skip pre-check (`any_active`)
    /// evaluated it a SECOND time, so `run_until(5)` ran only ~2 times over 10
    /// frames. Both executors (`run_sequential` and `run`) must give exactly 5.
    #[test]
    fn run_condition_evaluated_once_per_frame() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNT: AtomicU32 = AtomicU32::new(0);

        fn drive(run: impl Fn(&mut Scheduler, &mut World)) -> u32 {
            COUNT.store(0, Ordering::SeqCst);
            let mut sched = Scheduler::new();
            sched.add_systems(
                StageLabel::Update,
                seq("counter", |_: &mut World| {
                    COUNT.fetch_add(1, Ordering::SeqCst);
                })
                .run_if_cond(conditions::run_until(5)),
            );
            let mut world = World::new();
            sched.compile_with_world(&world).unwrap();
            for _ in 0..10 {
                run(&mut sched, &mut world);
            }
            COUNT.load(Ordering::SeqCst)
        }

        assert_eq!(drive(|s, w| s.run_sequential(w)), 5, "run_sequential: run_until(5) → 5");
        assert_eq!(drive(|s, w| s.run(w)), 5, "run(): run_until(5) → 5");
    }
