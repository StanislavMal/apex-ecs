/// Создаёт параллельную систему с автоматическим выводом AccessDescriptor.
///
/// # Вариант А — без состояния
///
/// ```ignore
/// system! {
///     fn movement_system(
///         q: (Read<Velocity>, Write<Position>),
///         keys: Res<Input<KeyCode>>,
///     ) {
///         for (vel, mut pos) in q.iter() {
///             if keys.pressed(KeyCode::A) { pos.x -= vel.x; }
///         }
///     }
/// }
/// // Регистрация: app.add_system(Update, movement_system);
/// ```
///
/// # Вариант Б — с состоянием
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
/// // Регистрация: app.add_system(Update, Spawner::default());
/// ```
///
/// # Параметры
///
/// | Параметр | Тип доступа | Описание |
/// |----------|------------|----------|
/// | `q: (Read<A>, Write<B>)` | Query (кортеж) | Итерация по компонентам |
/// | `q: Read<A>` | Query (одиночный) | Итерация по одному компоненту |
/// | `name: Res<T>` | ResRead\<T\> | Иммутабельный ресурс (П2; `&T` — compile-ошибка: у Bevy `&T` = компонент) |
/// | `name: ResMut<T>` | ResWrite\<T\> | Мутабельный ресурс (П2; `&mut T` — compile-ошибка) |
/// | `name: &[E]` / `EventReader<E>` | Listen\<E\> | Чтение событий |
/// | `name: &mut Vec<E>` / `EventWriter<E>` | Emit\<E\> | Отправка событий (`.send()`) |
/// | `name: Cmd` | Commands | Отложенные структурные изменения |
/// | `name: Ctx` | SystemContext | Прямой доступ к контексту |
/// | `__whole: WholeWorld` | NEEDS_WHOLE_WORLD | Глобальный доступ ко всем entity |
///
/// # Only one Query parameter (F6)
///
/// Every query parameter accumulates into one `Self::Query`, so a second query
/// would silently become the same joined AND query rather than an independent
/// one. Two query parameters are therefore a compile error — combine them:
///
/// ```compile_fail
/// use apex_core::system;
/// use apex_core::query::Read;
/// struct A;
/// impl apex_core::component::Component for A {}
/// struct B;
/// impl apex_core::component::Component for B {}
/// system! {
///     fn two_queries(q1: (Read<A>,), q2: (Read<B>,)) {
///         let _ = (q1, q2);
///     }
/// }
/// ```
#[macro_export]
macro_rules! system {
    // ═══════════════════════════════════════════════════════════════
    //  Эксклюзивные системы: `world: &mut World` ⇒ FULL access ⇒ alone
    // ═══════════════════════════════════════════════════════════════

    // ── Exclusive A: stateless `fn name(world: &mut World) { … }` ──
    {
        fn $fn_name:ident( $world:ident : &mut World $(,)? ) {
            $($body:tt)*
        }
    } => {
        #[allow(non_camel_case_types, dead_code)]
        struct $fn_name;
        impl $crate::ExclusiveSystem for $fn_name {
            fn run(&mut self, $world: &mut $crate::World) { $($body)* }
            fn name(&self) -> &'static str { stringify!($fn_name) }
        }
    };

    // ── Guard: `world` + другие параметры → понятная ошибка (U.1) ──
    {
        fn $fn_name:ident( $world:ident : &mut World , $($rest:tt)+ ) {
            $($body:tt)*
        }
    } => {
        compile_error!(concat!(
            "`", stringify!($world), ": &mut World` — это эксклюзивная система с FULL access;\n",
            "её нельзя комбинировать с другими параметрами (она и так даёт полный доступ ко всему миру).\n",
            "Внутри тела используй world.resource(), world.query::<_>(), world.spawn(...) напрямую."
        ));
    };

    // ── Exclusive B: stateful с дефолтами ──
    {
        struct $struct_name:ident {
            $( $field:ident : $fty:ty = $default:expr ),+ $(,)?
        }
        fn run( $slf:ident : &mut Self , $world:ident : &mut World $(,)? ) {
            $($body:tt)*
        }
    } => {
        struct $struct_name { $( $field: $fty ),+ }
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

    // ── Exclusive B': stateful без дефолтов (U.5 — поля pub, без Default) ──
    {
        struct $struct_name:ident {
            $( $field:ident : $fty:ty ),+ $(,)?
        }
        fn run( $slf:ident : &mut Self , $world:ident : &mut World $(,)? ) {
            $($body:tt)*
        }
    } => {
        struct $struct_name { $( pub $field: $fty ),+ }
        impl $crate::ExclusiveSystem for $struct_name {
            fn run(&mut self, $world: &mut $crate::World) {
                let $slf = &mut *self;
                $($body)*
            }
            fn name(&self) -> &'static str { stringify!($struct_name) }
        }
    };

    // ═══════════════════════════════════════════════════════════════
    //  Параллельные системы: доступ выведен из параметров
    // ═══════════════════════════════════════════════════════════════

    // ── Parallel A: stateless ──
    {
        fn $fn_name:ident(
            $($params:tt)*
        ) {
            $($body:tt)*
        }
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: ctx,
            @q: [], @r: [], @e: [],
            @before: [], @after: [],
            @params: [ $($params)* ],
            @body: { $($body)* },
            @struct_body: [],
            @slf: [], @whole: [], @cmd: [],
        }
    };

    // ── Parallel B: with state (с дефолтами — генерируется Default) ──
    {
        struct $struct_name:ident {
            $( $field:ident : $fty:ty = $default:expr ),+ $(,)?
        }
        fn run(
            $slf:ident : &mut Self,
            $($params:tt)*
        ) {
            $($body:tt)*
        }
    } => {
        struct $struct_name { $( $field: $fty ),+ }

        impl Default for $struct_name {
            fn default() -> Self { Self { $( $field: $default ),+ } }
        }

        $crate::__system_impl! {
            @fn_name: $struct_name,
            @ctx: ctx,
            @q: [], @r: [], @e: [],
            @before: [], @after: [],
            @params: [ $($params)* ],
            @body: { $($body)* },
            @struct_body: [ struct $struct_name { $( $field: $fty ),+ } ],
            @slf: [ $slf ], @whole: [], @cmd: [],
        }
    };

    // ── Parallel B': with state без дефолтов (U.5 — поля pub, без Default) ──
    {
        struct $struct_name:ident {
            $( $field:ident : $fty:ty ),+ $(,)?
        }
        fn run(
            $slf:ident : &mut Self,
            $($params:tt)*
        ) {
            $($body:tt)*
        }
    } => {
        struct $struct_name { $( pub $field: $fty ),+ }

        $crate::__system_impl! {
            @fn_name: $struct_name,
            @ctx: ctx,
            @q: [], @r: [], @e: [],
            @before: [], @after: [],
            @params: [ $($params)* ],
            @body: { $($body)* },
            @struct_body: [ struct $struct_name { $( $field: $fty ),+ } ],
            @slf: [ $slf ], @whole: [], @cmd: [],
        }
    };
}

// ── Helpers ──────────────────────────────────────────────────────

#[doc(hidden)]
#[macro_export]
macro_rules! __emit_struct {
    { [] $fn_name:ident } => { #[allow(non_camel_case_types, dead_code)] struct $fn_name; };
    { [ $($t:tt)+ ] $fn_name:ident } => {};
}

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

#[doc(hidden)]
#[macro_export]
macro_rules! __sys_compile_error {
    ( $first:tt $($rest:tt)* ) => {
        compile_error!(concat!(
            "unsupported parameter in system! macro: \"",
            stringify!($first),
            "\"\n\n\
            Expected one of:\n  \
            - q: (Read<A>, Write<B>) — query (tuple)\n  \
            - q: Read<A>             — query (single)\n  \
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
    // Base case
    {
        @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [], @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
        @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => {
        $crate::__emit_struct! { [ $( $struct_tokens )* ] $fn_name }
        impl $crate::AutoSystem for $fn_name {
            type Query = ( $( $($q)+ ),* );
            type Resources = ( $( $($r)+ ),* );
            type Events = ( $( $($e)+ ),* );
            $crate::__sys_whole_world!([ $( $whole )* ]);
            $crate::__sys_has_deferred!([ $( $cmd )* ]);
            fn run(&mut self, $ctx: $crate::SystemContext<'_>) {
                $( let $slf_name = &mut *self; )*
                $( $before )* $( $body )* $( $after )*
            }
            fn name() -> &'static str { stringify!($fn_name) }
        }
    };

    // ═══ With trailing comma ═══

    // Ctx
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Ctx , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &$crate::SystemContext<'_> = &$ctx; ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ X ], @cmd: [ $( $cmd )* ],
    }};

    // WholeWorld
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : WholeWorld , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* ], @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ X ], @cmd: [ $( $cmd )* ],
    }};

    // F6: a SECOND Query parameter is rejected. All query params accumulate into
    // `@q` and every one binds to `$ctx.query::<Self::Query>()` — the tuple of ALL
    // of them — so two query params would silently become the same joined AND
    // query, not two independent queries. Matches when `@q` already holds one
    // query and another arrives (trailing comma optional). Tried before the
    // general query arms below, so the FIRST query (empty `@q`) falls through.
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ ( $($q0:tt)+ ) $( ( $($qn:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ( $( $qty:tt )* ) $( , $( $rest:tt )* )? ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => {
        compile_error!(
            "system! supports only one Query parameter — combine components into a single tuple query, e.g. `q: (Read<A>, Write<B>)`"
        );
    };

    // Query tuple
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ( $( $qty:tt )* ) , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ( $( $qty )* ) ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname = $ctx.query::<Self::Query>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Event reader
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & [ $ev:ty ] , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Listen<$ev> ) ],
        @before: [ $( $before )* let $pname = $ctx.event_reader::<$ev>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Event writer (generates EventWriter, user calls .send())
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut Vec < $ev:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Emit<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_writer::<$ev>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Resource write — Res/ResMut-семантика (П2): та же грамматика, что в
    // plain-fn системах. Bare `&T`/`&mut T` как ресурс БОЛЬШЕ НЕ принимаются
    // (compile-ошибка ниже) — у Bevy `&T` означает компонент запроса,
    // двойная семантика была ловушкой мигранта.
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ResMut < $ty:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResWrite<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let mut $pname: $crate::system_param::ResMut<'_, $ty> = $ctx.resource_mut::<$ty>(); let _ = &mut $pname; ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Resource read — Res<T>
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Res < $ty:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResRead<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: $crate::system_param::Res<'_, $ty> = $ctx.resource::<$ty>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // EventReader<E> — Bevy-имя для чтения событий (эквивалент `&[E]`)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : EventReader < $ev:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Listen<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_reader::<$ev>(); let _ = &mut $pname; ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // EventWriter<E> — Bevy-имя для записи событий (эквивалент `&mut Vec<E>`)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : EventWriter < $ev:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Emit<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_writer::<$ev>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // ── П2: bare `&T`/`&mut T` как ресурс — БОЛЬШЕ НЕ ПОДДЕРЖИВАЕТСЯ ──
    // (двойная семантика с Bevy-компонентами была ловушкой №1 мигранта)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut $ty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { compile_error!(concat!(
        "system!: `", stringify!($pname), ": &mut ", stringify!($ty),
        "` — `&mut T` больше не означает ресурс (ловушка Bevy-семантики, П2).\n\
         Используйте `", stringify!($pname), ": ResMut<", stringify!($ty), ">`.\n\
         Запись событий — `имя: &mut Vec<E>` или `имя: EventWriter<E>`."
    )); };

    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & $ty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { compile_error!(concat!(
        "system!: `", stringify!($pname), ": &", stringify!($ty),
        "` — `&T` больше не означает ресурс (ловушка Bevy-семантики, П2).\n\
         Используйте `", stringify!($pname), ": Res<", stringify!($ty), ">`.\n\
         Чтение событий — `имя: &[E]` или `имя: EventReader<E>`;\n\
         компоненты — внутри запроса: `q: (Read<", stringify!($ty), ">, …)`."
    )); };

    // Commands
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Cmd , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &mut apex_core::Commands = $ctx.commands(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [X],
    }};

    // Single component query (bare type)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : $qty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ( $qty ) ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname = $ctx.query::<Self::Query>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // ═══ Without trailing comma (last param) ═══

    // Ctx (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Ctx ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &$crate::SystemContext<'_> = &$ctx; ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // WholeWorld (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : WholeWorld ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* ], @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ X ], @cmd: [ $( $cmd )* ],
    }};

    // Query tuple (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ( $( $qty:tt )* ) ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ( $( $qty )* ) ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname = $ctx.query::<Self::Query>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Event reader (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & [ $ev:ty ] ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Listen<$ev> ) ],
        @before: [ $( $before )* let $pname = $ctx.event_reader::<$ev>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Event writer (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut Vec < $ev:ty > ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Emit<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_writer::<$ev>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Resource write (last) — ResMut<T> (П2)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ResMut < $ty:ty > ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResWrite<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let mut $pname: $crate::system_param::ResMut<'_, $ty> = $ctx.resource_mut::<$ty>(); let _ = &mut $pname; ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Resource read (last) — Res<T> (П2)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Res < $ty:ty > ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResRead<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: $crate::system_param::Res<'_, $ty> = $ctx.resource::<$ty>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // EventReader<E> (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : EventReader < $ev:ty > ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Listen<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_reader::<$ev>(); let _ = &mut $pname; ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // EventWriter<E> (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : EventWriter < $ev:ty > ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Emit<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_writer::<$ev>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // ── П2: bare `&T`/`&mut T` как ресурс (last) — compile-ошибка ──
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut $ty:ty ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { compile_error!(concat!(
        "system!: `", stringify!($pname), ": &mut ", stringify!($ty),
        "` — `&mut T` больше не означает ресурс (ловушка Bevy-семантики, П2).\n\
         Используйте `", stringify!($pname), ": ResMut<", stringify!($ty), ">`.\n\
         Запись событий — `имя: &mut Vec<E>` или `имя: EventWriter<E>`."
    )); };

    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & $ty:ty ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { compile_error!(concat!(
        "system!: `", stringify!($pname), ": &", stringify!($ty),
        "` — `&T` больше не означает ресурс (ловушка Bevy-семантики, П2).\n\
         Используйте `", stringify!($pname), ": Res<", stringify!($ty), ">`.\n\
         Чтение событий — `имя: &[E]` или `имя: EventReader<E>`;\n\
         компоненты — внутри запроса: `q: (Read<", stringify!($ty), ">, …)`."
    )); };

    // Commands (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Cmd ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &mut apex_core::Commands = $ctx.commands(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [X],
    }};

    // Bare type query (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : $qty:ty ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ( $qty ) ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname = $ctx.query::<Self::Query>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Catch-all
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $($rest:tt)+ ], @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ], @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__sys_compile_error! { $($rest)* } };
}
