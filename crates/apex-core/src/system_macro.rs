/// Creates a parallel system with automatic AccessDescriptor inference.
///
/// # Variant A — stateless
///
/// ```ignore
/// system! {
///     fn movement_system(
///         q: (&Velocity, &mut Position),
///         keys: Res<Input<KeyCode>>,
///     ) {
///         // A `&mut` element ⇒ mutable iteration: `iter_mut` / `for_each_mut`
///         // (`iter`/`for_each` are read-only; see `Query::iter`). `q` is
///         // already `mut`-bound by this macro.
///         for (vel, mut pos) in q.iter_mut() {
///             if keys.pressed(KeyCode::A) { pos.x -= vel.x; }
///         }
///     }
/// }
/// // Registration: app.add_system(Update, movement_system);
/// ```
///
/// The stateless form declares a FUNCTION, `fn movement_system() -> impl AutoSystem` (the
/// exclusive one, `impl ExclusiveSystem`), not a unit struct: a function name never captures a
/// pattern, so a binding spelled like a system (`|_, (pilot,)|` next to a system `pilot`) still
/// compiles (ADR-022). It is passed by name as before — `add_systems`, `sys("name", f)`,
/// `.before(f)`/`.after(f)` — and registers under the function's name.
///
/// Every form takes outer attributes (doc comments, `#[cfg]`) and a visibility, carried to the
/// generated function or struct: `system! { /// Moves. pub fn movement(…) { … } }`. A `pub`
/// stateful struct can be no more visible than the components its `AutoSystem` impl names
/// (E0446); the function form hides them behind `impl AutoSystem`.
///
/// # Variant B — stateful
///
/// ```ignore
/// system! {
///     struct Spawner {
///         wave: u32 = 1,
///         count: u32 = 0,
///     }
///     fn run(s: &mut Self, cmd: Cmd, ctx: Ctx) {
///         if s.wave <= 5 {
///             cmd.spawn((Enemy, Position::default()));
///             s.count += 1;
///         }
///     }
/// }
/// // Registration: app.add_system(Update, Spawner::default());
/// ```
///
/// # Parameters
///
/// | Parameter | Access type | Description |
/// |----------|------------|----------|
/// | `q: (&A, &mut B)` | Query (tuple) | Iterate over components (the macro adds the `'static` spelling the associated type needs) |
/// | `q: (&A,)` | Query (single) | Iterate over a single component — a 1-tuple (bare `&A`/`&mut A` params are the resource trap below) |
/// | `name: Res<T>` | ResRead\<T\> | Immutable resource (P2; `&T` — compile error: in Bevy `&T` = component) |
/// | `name: ResMut<T>` | ResWrite\<T\> | Mutable resource (P2; `&mut T` — compile error) |
/// | `name: &[E]` / `EventReader<E>` | Listen\<E\> | Read events |
/// | `name: &mut Vec<E>` / `EventWriter<E>` | Emit\<E\> | Send events (`.send()`) |
/// | `name: Cmd` | Commands | Deferred structural changes |
/// | `name: Ctx` | SystemContext | Direct access to the context |
/// | `__whole: WholeWorld` | NEEDS_WHOLE_WORLD | Global access to all entities |
///
/// # Event readers hold a persistent per-system cursor (F4b)
///
/// An event-reader parameter (`name: &[E]` / `name: EventReader<E>`) reads through
/// a PERSISTENT per-system cursor that resumes across runs — so if the system runs
/// several times within one frame without an intervening flush (a `FixedUpdate`
/// catch-up), each event is read exactly once, no duplicates. This matches the
/// plain-fn `EventReader<E>` golden path (F4); the cursor is owned by the system's
/// scheduler adapter and installed into the `SystemContext` per run.
///
/// Because a shared cursor cannot be split across ASD row-split chunks, an
/// event-reading system is automatically single-task (`stateful`) — it is never
/// row-split. Two readers of the SAME event type in one system share one cursor
/// (read-once total).
///
/// # Only one Query parameter (F6)
///
/// Every query parameter accumulates into one `Self::Query`, so a second query
/// would silently become the same joined AND query rather than an independent
/// one. Two query parameters are therefore a compile error — combine them:
///
/// ```compile_fail
/// use apex_core::system;
/// struct A;
/// impl apex_core::component::Component for A {}
/// struct B;
/// impl apex_core::component::Component for B {}
/// system! {
///     fn two_queries(q1: (&A,), q2: (&B,)) {
///         let _ = (q1, q2);
///     }
/// }
/// ```
#[macro_export]
macro_rules! system {
    // Every form takes outer attributes (doc comments included) and a visibility, carried to
    // the generated item: `system! { /// Moves. pub fn movement(…) { … } }` (ADR-022).

    // ═══════════════════════════════════════════════════════════════
    //  Exclusive systems: `world: &mut World` ⇒ FULL access ⇒ alone
    // ═══════════════════════════════════════════════════════════════

    // ── Exclusive A: stateless `fn name(world: &mut World) { … }` ──
    // A FUNCTION returning the system, not a unit struct named `name`: a unit struct sits in the
    // pattern namespace and broke every binding spelled like the system (ADR-022).
    {
        $(#[$attr:meta])*
        $vis:vis fn $fn_name:ident( $world:ident : &mut World $(,)? ) {
            $($body:tt)*
        }
    } => {
        $(#[$attr])*
        #[allow(dead_code)]
        $vis fn $fn_name() -> impl $crate::ExclusiveSystem {
            struct __ApexSystemFn;
            impl $crate::ExclusiveSystem for __ApexSystemFn {
                fn run(&mut self, $world: &mut $crate::World) { $($body)* }
                fn name(&self) -> &'static str { stringify!($fn_name) }
            }
            __ApexSystemFn
        }
    };

    // ── Guard: `world` + other parameters → clear error (U.1) ──
    {
        $(#[$attr:meta])*
        $vis:vis fn $fn_name:ident( $world:ident : &mut World , $($rest:tt)+ ) {
            $($body:tt)*
        }
    } => {
        compile_error!(concat!(
            "`", stringify!($world), ": &mut World` is an exclusive system with FULL access;\n",
            "it cannot be combined with other parameters (it already grants full access to the whole world).\n",
            "Inside the body, use world.resource(), world.query::<_>(), world.spawn(...) directly."
        ));
    };

    // ── Exclusive B: stateful with defaults ──
    {
        $(#[$attr:meta])*
        $vis:vis struct $struct_name:ident {
            $( $field:ident : $fty:ty = $default:expr ),+ $(,)?
        }
        fn run( $slf:ident : &mut Self , $world:ident : &mut World $(,)? ) {
            $($body:tt)*
        }
    } => {
        $(#[$attr])*
        $vis struct $struct_name { $( $field: $fty ),+ }
        impl Default for $struct_name {
            fn default() -> Self { Self { $( $field: $default ),+ } }
        }
        impl $crate::ExclusiveSystem for $struct_name {
            fn run(&mut self, $world: &mut $crate::World) {
                let $slf = &mut *self;
                $($body)*
            }
            fn name(&self) -> &'static str { stringify!($struct_name) }
        }
    };

    // ── Exclusive B': stateful without defaults (U.5 — fields pub, no Default) ──
    {
        $(#[$attr:meta])*
        $vis:vis struct $struct_name:ident {
            $( $field:ident : $fty:ty ),+ $(,)?
        }
        fn run( $slf:ident : &mut Self , $world:ident : &mut World $(,)? ) {
            $($body:tt)*
        }
    } => {
        $(#[$attr])*
        $vis struct $struct_name { $( pub $field: $fty ),+ }
        impl $crate::ExclusiveSystem for $struct_name {
            fn run(&mut self, $world: &mut $crate::World) {
                let $slf = &mut *self;
                $($body)*
            }
            fn name(&self) -> &'static str { stringify!($struct_name) }
        }
    };

    // ═══════════════════════════════════════════════════════════════
    //  Parallel systems: access inferred from parameters
    // ═══════════════════════════════════════════════════════════════

    // ── Parallel A: stateless — a function returning the system (ADR-022) ──
    {
        $(#[$attr:meta])*
        $vis:vis fn $fn_name:ident(
            $($params:tt)*
        ) {
            $($body:tt)*
        }
    } => {
        $crate::__system_impl! {
            @head: [ fn [ $(#[$attr])* ] $vis $fn_name ],
            @ctx: ctx,
            @q: [], @r: [], @e: [],
            @before: [], @after: [],
            @params: [ $($params)* ],
            @body: { $($body)* },
            @slf: [], @whole: [], @cmd: [],
        }
    };

    // ── Parallel B: with state (with defaults — Default is generated) ──
    {
        $(#[$attr:meta])*
        $vis:vis struct $struct_name:ident {
            $( $field:ident : $fty:ty = $default:expr ),+ $(,)?
        }
        fn run(
            $slf:ident : &mut Self,
            $($params:tt)*
        ) {
            $($body:tt)*
        }
    } => {
        $(#[$attr])*
        $vis struct $struct_name { $( $field: $fty ),+ }

        impl Default for $struct_name {
            fn default() -> Self { Self { $( $field: $default ),+ } }
        }

        $crate::__system_impl! {
            @head: [ struct $struct_name ],
            @ctx: ctx,
            @q: [], @r: [], @e: [],
            @before: [], @after: [],
            @params: [ $($params)* ],
            @body: { $($body)* },
            @slf: [ $slf ], @whole: [], @cmd: [],
        }
    };

    // ── Parallel B': with state without defaults (U.5 — fields pub, no Default) ──
    {
        $(#[$attr:meta])*
        $vis:vis struct $struct_name:ident {
            $( $field:ident : $fty:ty ),+ $(,)?
        }
        fn run(
            $slf:ident : &mut Self,
            $($params:tt)*
        ) {
            $($body:tt)*
        }
    } => {
        $(#[$attr])*
        $vis struct $struct_name { $( pub $field: $fty ),+ }

        $crate::__system_impl! {
            @head: [ struct $struct_name ],
            @ctx: ctx,
            @q: [], @r: [], @e: [],
            @before: [], @after: [],
            @params: [ $($params)* ],
            @body: { $($body)* },
            @slf: [ $slf ], @whole: [], @cmd: [],
        }
    };
}

// ── Helpers ──────────────────────────────────────────────────────

#[doc(hidden)]
#[macro_export]
macro_rules! __sys_whole_world {
    ( [] ) => {};
    ( [ $($t:tt)+ ] ) => {
        const NEEDS_WHOLE_WORLD: bool = true;
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __sys_has_deferred {
    ( [] ) => {};
    ( [ $($t:tt)+ ] ) => {
        const HAS_DEFERRED: bool = true;
    };
}

/// Rewrites a comma-separated query-element list into the tuple type the
/// lifetime-free `AutoSystem::Query` associated type needs: `&T` → `&'static T`,
/// `&mut T` → `&'static mut T`, plain elements (`Entity`, `Maybe<T>`, filters)
/// pass through. The `'static` is the canonical associated-type spelling — the
/// borrow is re-bound to the world's lifetime at fetch via `WorldQuery::Item<'_>`.
/// Always emits a real tuple (trailing comma), so a single element is a 1-tuple.
#[doc(hidden)]
#[macro_export]
macro_rules! __q_static {
    // ── @parse arms first: the entry arm below matches anything ──
    (@parse [ $($out:tt)* ]) => { ( $($out)* ) };
    (@parse [ $($out:tt)* ] & mut $ty:ty) => { ( $($out)* &'static mut $ty, ) };
    (@parse [ $($out:tt)* ] & mut $ty:ty , $($rest:tt)*) => {
        $crate::__q_static!(@parse [ $($out)* &'static mut $ty, ] $($rest)*)
    };
    (@parse [ $($out:tt)* ] & $ty:ty) => { ( $($out)* &'static $ty, ) };
    (@parse [ $($out:tt)* ] & $ty:ty , $($rest:tt)*) => {
        $crate::__q_static!(@parse [ $($out)* &'static $ty, ] $($rest)*)
    };
    (@parse [ $($out:tt)* ] $ty:ty) => { ( $($out)* $ty, ) };
    (@parse [ $($out:tt)* ] $ty:ty , $($rest:tt)*) => {
        $crate::__q_static!(@parse [ $($out)* $ty, ] $($rest)*)
    };
    // Entry.
    ( $($input:tt)* ) => { $crate::__q_static!(@parse [] $($input)*) };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __sys_compile_error {
    ( $first:tt $($rest:tt)* ) => {
        compile_error!(concat!(
            "unsupported parameter in system! macro: \"",
            stringify!($first),
            "\"\n\n\
            Expected one of:\n  \
            - q: (&A, &mut B)        — query (tuple)\n  \
            - q: (&A,)               — query (single component, a 1-tuple)\n  \
            - name: Res<T>           — resource read\n  \
            - name: ResMut<T>        — resource write\n  \
            - name: &[E]             — event reader\n  \
            - name: &mut Vec<E>      — event writer (use .send())\n  \
            - cmd: Cmd               — commands\n  \
            - ctx: Ctx               — SystemContext access\n  \
            - __whole: WholeWorld    — NEEDS_WHOLE_WORLD flag"
        ));
    };
}

// ── Core impl ────────────────────────────────────────────────────

#[doc(hidden)]
#[macro_export]
macro_rules! __system_impl {
    // Base case — a stateful system: the struct was emitted by the entry arm.
    {
        @head: [ struct $name:ident ], @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [], @body: { $( $body:tt )* },
        @slf: [ $( $slf_name:ident )* ],
        @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => {
        impl $crate::AutoSystem for $name {
            type Query = $crate::__q_static!( $( $($q)+ ),* );
            type Resources = ( $( $($r)+ ),* );
            type Events = ( $( $($e)+ ),* );
            $crate::__sys_whole_world!([ $( $whole )* ]);
            $crate::__sys_has_deferred!([ $( $cmd )* ]);
            fn run(&mut self, $ctx: $crate::SystemContext<'_>) {
                $( let $slf_name = &mut *self; )*
                $( $before )* $( $body )* $( $after )*
            }
            fn name() -> &'static str { stringify!($name) }
        }
    };

    // Base case — a stateless system: a function returning a zero-sized system local to it
    // (ADR-022). The name the scheduler registers and orders by stays the function's.
    {
        @head: [ fn [ $( $attr:tt )* ] $vis:vis $name:ident ], @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [], @body: { $( $body:tt )* },
        @slf: [],
        @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => {
        $( $attr )*
        #[allow(dead_code)]
        $vis fn $name() -> impl $crate::AutoSystem {
            struct __ApexSystemFn;
            impl $crate::AutoSystem for __ApexSystemFn {
                type Query = $crate::__q_static!( $( $($q)+ ),* );
                type Resources = ( $( $($r)+ ),* );
                type Events = ( $( $($e)+ ),* );
                $crate::__sys_whole_world!([ $( $whole )* ]);
                $crate::__sys_has_deferred!([ $( $cmd )* ]);
                fn run(&mut self, $ctx: $crate::SystemContext<'_>) {
                    $( $before )* $( $body )* $( $after )*
                }
                fn name() -> &'static str { stringify!($name) }
            }
            __ApexSystemFn
        }
    };

    // ═══ With trailing comma ═══

    // Ctx
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Ctx , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &$crate::SystemContext<'_> = &$ctx; ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ X ], @cmd: [ $( $cmd )* ],
    }};

    // WholeWorld
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : WholeWorld , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* ], @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ X ], @cmd: [ $( $cmd )* ],
    }};

    // F6: a SECOND Query parameter is rejected. All query params accumulate into
    // `@q` and every one binds to `$ctx.query::<Self::Query>()` — the tuple of ALL
    // of them — so two query params would silently become the same joined AND
    // query, not two independent queries. Matches when `@q` already holds one
    // query and another arrives (trailing comma optional). Tried before the
    // general query arms below, so the FIRST query (empty `@q`) falls through.
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ ( $($q0:tt)+ ) $( ( $($qn:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ( $( $qty:tt )* ) $( , $( $rest:tt )* )? ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => {
        compile_error!(
            "system! supports only one Query parameter — combine components into a single tuple query, e.g. `q: (Read<A>, Write<B>)`"
        );
    };

    // Query tuple
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ( $( $qty:tt )* ) , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ( $( $qty )* ) ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* #[allow(unused_mut)] let mut $pname = $ctx.query_unchecked::<Self::Query>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Event reader
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & [ $ev:ty ] , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Listen<$ev> ) ],
        @before: [ $( $before )* let $pname = $ctx.event_reader_persistent_auto::<$ev>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Event writer (generates EventWriter, user calls .send())
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut Vec < $ev:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Emit<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_writer_unchecked::<$ev>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Resource write — Res/ResMut semantics (P2): same grammar as in
    // plain-fn systems. Bare `&T`/`&mut T` as a resource is NO LONGER accepted
    // (compile error below) — in Bevy `&T` denotes a query component,
    // the dual semantics was a migrant trap.
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ResMut < $ty:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResWrite<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let mut $pname: $crate::system_param::ResMut<'_, $ty> = $ctx.resource_mut_unchecked::<$ty>(); let _ = &mut $pname; ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Resource read — Res<T>
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Res < $ty:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResRead<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: $crate::system_param::Res<'_, $ty> = $ctx.resource::<$ty>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // EventReader<E> — Bevy name for reading events (equivalent to `&[E]`)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : EventReader < $ev:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Listen<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_reader_persistent_auto::<$ev>(); let _ = &mut $pname; ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // EventWriter<E> — Bevy name for writing events (equivalent to `&mut Vec<E>`)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : EventWriter < $ev:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Emit<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_writer_unchecked::<$ev>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // ── P2: bare `&T`/`&mut T` as a resource — NO LONGER SUPPORTED ──
    // (the dual semantics with Bevy components was the migrant's trap #1)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut $ty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { compile_error!(concat!(
        "system!: `", stringify!($pname), ": &mut ", stringify!($ty),
        "` — `&mut T` no longer denotes a resource (Bevy-semantics trap, P2).\n\
         Use `", stringify!($pname), ": ResMut<", stringify!($ty), ">`.\n\
         Event writing — `name: &mut Vec<E>` or `name: EventWriter<E>`;\n\
         a single-component query — a 1-tuple: `q: (&mut ", stringify!($ty), ",)`."
    )); };

    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & $ty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { compile_error!(concat!(
        "system!: `", stringify!($pname), ": &", stringify!($ty),
        "` — `&T` no longer denotes a resource (Bevy-semantics trap, P2).\n\
         Use `", stringify!($pname), ": Res<", stringify!($ty), ">`.\n\
         Event reading — `name: &[E]` or `name: EventReader<E>`;\n\
         components — inside a query (a 1-tuple for one component): `q: (&", stringify!($ty), ",)`."
    )); };

    // Commands
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Cmd , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &mut $crate::Commands = $ctx.commands(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [X],
    }};

    // Single component query (bare type)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : $qty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ( $qty ) ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* #[allow(unused_mut)] let mut $pname = $ctx.query_unchecked::<Self::Query>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // ═══ Without trailing comma (last param) ═══

    // Ctx (last)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Ctx ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &$crate::SystemContext<'_> = &$ctx; ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // WholeWorld (last)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : WholeWorld ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* ], @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ X ], @cmd: [ $( $cmd )* ],
    }};

    // Query tuple (last)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ( $( $qty:tt )* ) ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ( $( $qty )* ) ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* #[allow(unused_mut)] let mut $pname = $ctx.query_unchecked::<Self::Query>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Event reader (last)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & [ $ev:ty ] ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Listen<$ev> ) ],
        @before: [ $( $before )* let $pname = $ctx.event_reader_persistent_auto::<$ev>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Event writer (last)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut Vec < $ev:ty > ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Emit<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_writer_unchecked::<$ev>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Resource write (last) — ResMut<T> (P2)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ResMut < $ty:ty > ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResWrite<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let mut $pname: $crate::system_param::ResMut<'_, $ty> = $ctx.resource_mut_unchecked::<$ty>(); let _ = &mut $pname; ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Resource read (last) — Res<T> (P2)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Res < $ty:ty > ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResRead<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: $crate::system_param::Res<'_, $ty> = $ctx.resource::<$ty>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // EventReader<E> (last)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : EventReader < $ev:ty > ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Listen<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_reader_persistent_auto::<$ev>(); let _ = &mut $pname; ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // EventWriter<E> (last)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : EventWriter < $ev:ty > ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Emit<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_writer_unchecked::<$ev>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // ── P2: bare `&T`/`&mut T` as a resource (last) — compile error ──
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut $ty:ty ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { compile_error!(concat!(
        "system!: `", stringify!($pname), ": &mut ", stringify!($ty),
        "` — `&mut T` no longer denotes a resource (Bevy-semantics trap, P2).\n\
         Use `", stringify!($pname), ": ResMut<", stringify!($ty), ">`.\n\
         Event writing — `name: &mut Vec<E>` or `name: EventWriter<E>`;\n\
         a single-component query — a 1-tuple: `q: (&mut ", stringify!($ty), ",)`."
    )); };

    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & $ty:ty ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { compile_error!(concat!(
        "system!: `", stringify!($pname), ": &", stringify!($ty),
        "` — `&T` no longer denotes a resource (Bevy-semantics trap, P2).\n\
         Use `", stringify!($pname), ": Res<", stringify!($ty), ">`.\n\
         Event reading — `name: &[E]` or `name: EventReader<E>`;\n\
         components — inside a query (a 1-tuple for one component): `q: (&", stringify!($ty), ",)`."
    )); };

    // Commands (last)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Cmd ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &mut $crate::Commands = $ctx.commands(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [X],
    }};

    // Bare type query (last)
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : $qty:ty ],
        @body: { $( $body:tt )* }, 
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @head: $head, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ( $qty ) ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* #[allow(unused_mut)] let mut $pname = $ctx.query_unchecked::<Self::Query>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Catch-all
    { @head: $head:tt, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $($rest:tt)+ ], @body: { $( $body:tt )* },
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__sys_compile_error! { $($rest)* } };
}
