/// Создаёт параллельную систему с автоматическим выводом AccessDescriptor.
///
/// # Вариант А — без состояния
///
/// ```ignore
/// system! {
///     fn movement_system(
///         q: (Read<Velocity>, Write<Position>),
///         keys: &Input<KeyCode>,
///     ) {
///         for (_, (vel, pos)) in q.iter() {
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
/// | `name: &T` | ResRead\<T\> | Иммутабельный ресурс |
/// | `name: &mut T` | ResWrite\<T\> | Мутабельный ресурс |
/// | `name: &[E]` | Listen\<E\> | Чтение событий |
/// | `name: &mut Vec<E>` | Emit\<E\> | Отправка событий (`.send()`) |
/// | `name: Cmd` | Commands | Отложенные структурные изменения |
/// | `name: Ctx` | SystemContext | Прямой доступ к контексту |
/// | `__whole: WholeWorld` | NEEDS_WHOLE_WORLD | Глобальный доступ ко всем entity |
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
            - name: &T               — resource read\n  \
            - name: &mut T           — resource write\n  \
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
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
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

    // Resource write
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut $ty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResWrite<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &mut $ty = &mut *$ctx.resource_mut::<$ty>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Resource read
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & $ty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResRead<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &$ty = &*$ctx.resource::<$ty>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

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

    // Resource write (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut $ty:ty ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResWrite<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &mut $ty = &mut *$ctx.resource_mut::<$ty>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

    // Resource read (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & $ty:ty ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ], @cmd: [ $( $cmd:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResRead<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &$ty = &*$ctx.resource::<$ty>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ], @cmd: [ $( $cmd )* ],
    }};

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
