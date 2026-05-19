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
    // ── Variant A: stateless ──
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
            @slf: [], @whole: [],
        }
    };

    // ── Variant B: with state ──
    {
        struct $struct_name:ident {
            $( $field:ident : $fty:ty = $default:expr ),* $(,)?
        }
        fn run(
            $slf:ident : &mut Self,
            $($params:tt)*
        ) {
            $($body:tt)*
        }
    } => {
        struct $struct_name { $( $field: $fty ),* }

        impl Default for $struct_name {
            fn default() -> Self { Self { $( $field: $default ),* } }
        }

        $crate::__system_impl! {
            @fn_name: $struct_name,
            @ctx: ctx,
            @q: [], @r: [], @e: [],
            @before: [], @after: [],
            @params: [ $($params)* ],
            @body: { $($body)* },
            @struct_body: [ struct $struct_name { $( $field: $fty ),* } ],
            @slf: [ $slf ], @whole: [],
        }
    };
}

// ── Helpers ──────────────────────────────────────────────────────

#[doc(hidden)] #[macro_export]
macro_rules! __emit_struct {
    { [] $fn_name:ident } => { #[allow(non_camel_case_types, dead_code)] struct $fn_name; };
    { [ $($t:tt)+ ] $fn_name:ident } => {};
}

#[doc(hidden)] #[macro_export]
macro_rules! __sys_whole_world {
    ( [] ) => {};
    ( [ $($t:tt)+ ] ) => { const NEEDS_WHOLE_WORLD: bool = true; };
}

#[doc(hidden)] #[macro_export]
macro_rules! __sys_compile_error {
    ( $first:tt $($rest:tt)* ) => {
        compile_error!(concat!(
            "unsupported parameter in system! macro: \"", stringify!($first), "\"\n\n\
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

#[doc(hidden)] #[macro_export]
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
        @whole: [ $( $whole:tt )* ],
    } => {
        $crate::__emit_struct! { [ $( $struct_tokens )* ] $fn_name }
        impl $crate::AutoSystem for $fn_name {
            type Query = ( $( $($q)+ ),* );
            type Resources = ( $( $($r)+ ),* );
            type Events = ( $( $($e)+ ),* );
            $crate::__sys_whole_world!([ $( $whole )* ]);
            fn run(&mut self, $ctx: $crate::SystemContext<'_>) {
                $( let $slf_name = &mut *self; )*
                $( $before )* $( $body )* $( $after )*
            }
        }
    };

    // ═══ With trailing comma ═══

    // Ctx
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Ctx , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &$crate::SystemContext<'_> = &$ctx; ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // WholeWorld
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : WholeWorld , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* ], @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ X ],
    }};

    // Query tuple
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ( $( $qty:tt )* ) , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ( $( $qty )* ) ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname = $ctx.query::<Self::Query>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Event reader
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & [ $ev:ty ] , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Listen<$ev> ) ],
        @before: [ $( $before )* let $pname = $ctx.event_reader::<$ev>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Event writer (generates EventWriter, user calls .send())
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut Vec < $ev:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Emit<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_writer::<$ev>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Resource write
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut $ty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResWrite<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &mut $ty = &mut *$ctx.resource_mut::<$ty>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Resource read
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & $ty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResRead<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &$ty = &*$ctx.resource::<$ty>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Commands
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Cmd , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &mut apex_core::Commands = $ctx.commands(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Single component query (bare type)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : $qty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ( $qty ) ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname = $ctx.query::<Self::Query>(); ],
        @after: [ $( $after )* ], @params: [ $( $rest )* ], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // ═══ Without trailing comma (last param) ═══

    // Ctx (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Ctx ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &$crate::SystemContext<'_> = &$ctx; ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // WholeWorld (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : WholeWorld ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* ], @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ X ],
    }};

    // Query tuple (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ( $( $qty:tt )* ) ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ( $( $qty )* ) ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname = $ctx.query::<Self::Query>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Event reader (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & [ $ev:ty ] ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Listen<$ev> ) ],
        @before: [ $( $before )* let $pname = $ctx.event_reader::<$ev>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Event writer (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut Vec < $ev:ty > ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ( Emit<$ev> ) ],
        @before: [ $( $before )* let mut $pname = $ctx.event_writer::<$ev>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Resource write (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut $ty:ty ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResWrite<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &mut $ty = &mut *$ctx.resource_mut::<$ty>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Resource read (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & $ty:ty ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ( ResRead<$ty> ) ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &$ty = &*$ctx.resource::<$ty>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Commands (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Cmd ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname: &mut apex_core::Commands = $ctx.commands(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Bare type query (last)
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : $qty:ty ],
        @body: { $( $body:tt )* }, @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__system_impl! { @fn_name: $fn_name, @ctx: $ctx,
        @q: [ $( ( $($q)+ ) )* ( $qty ) ], @r: [ $( ( $($r)+ ) )* ], @e: [ $( ( $($e)+ ) )* ],
        @before: [ $( $before )* let $pname = $ctx.query::<Self::Query>(); ],
        @after: [ $( $after )* ], @params: [], @body: { $( $body )* },
        @struct_body: [ $( $struct_tokens )* ], @slf: [ $( $slf_name )* ], @whole: [ $( $whole )* ],
    }};

    // Catch-all
    { @fn_name: $fn_name:ident, @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ], @r: [ $( ( $($r:tt)+ ) )* ], @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $($rest:tt)+ ], @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ], @slf: [ $( $slf_name:ident )* ], @whole: [ $( $whole:tt )* ],
    } => { $crate::__sys_compile_error! { $($rest)* } };
}

/// Создаёт последовательную систему с эксклюзивным `&mut World` доступом.
///
/// В отличие от [`system!`], sequential-системы выполняются строго по одной
/// и получают полный `&mut World`. Используются для структурных изменений:
/// деспавн, рекурсивные операции, Lua-скриптинг.
///
/// # Вариант А — без состояния
///
/// ```ignore
/// sequential_system! {
///     fn cleanup_dead(
///         world: &mut World,
///         q: Read<Health>,
///         cmd: Cmd,
///     ) {
///         for (entity, hp) in q.iter() {
///             if hp.current == 0 { cmd.despawn(entity); }
///         }
///         cmd.apply(world);  // ручной apply
///     }
/// }
/// // Регистрация: app.add_sequential_system(PostUpdate, "cleanup", cleanup_dead);
/// ```
///
/// # Вариант Б — с состоянием
///
/// ```ignore
/// sequential_system! {
///     struct FrameCounter {
///         calls: u64 = 0,
///     }
///     fn run(
///         s: &mut Self,
///         world: &mut World,
///         time: &Time,
///     ) {
///         s.calls += 1;
///         if s.calls % 60 == 0 {
///             println!("Frame {}", time.frame_count);
///         }
///     }
/// }
/// // Регистрация:
/// //   app.add_sequential_system(PostUpdate, "counter", FrameCounter::default().into_system());
/// ```
///
/// # Параметры
///
/// Все параметры из [`system!`] поддерживаются, плюс:
///
/// | Параметр | Описание |
/// |----------|----------|
/// | `world: &mut World` | **Обязателен.** Эксклюзивный доступ к миру |
/// | `name: Ctx` | `&World` для read-only методов |
/// | `name: Cmd` | `Commands` — требуется **ручной** `.apply(world)` |
///
/// # Ключевые отличия от [`system!`]
///
/// | | `system!` | `sequential_system!` |
/// |---|---|---|
/// | Доступ | `SystemContext<'_>` (чанкованный) | `&mut World` (полный) |
/// | Параллельность | Да (ASD) | Нет (строго последовательно) |
/// | `cmd: Cmd` | Авто-apply после stage | Ручной `cmd.apply(world)` |
/// | `ctx: Ctx` | `&SystemContext<'_>` | `&World` |
// ═══════════════════════════════════════════════════════════════════

#[macro_export]
macro_rules! sequential_system {
    // ── Variant A: stateless ──
    {
        fn $fn_name:ident(
            $($params:tt)*
        ) {
            $($body:tt)*
        }
    } => {
        $crate::__seq_system_impl! {
            @mode: fn,
            @fn_name: $fn_name,
            @world: world,
            @before: [],
            @after: [],
            @params: [ $($params)* ],
            @body: { $($body)* },
        }
    };

    // ── Variant B: with state ──
    // (world: &mut World — последний параметр)
    {
        struct $struct_name:ident {
            $( $field:ident : $fty:ty = $default:expr ),* $(,)?
        }
        fn run(
            $slf:ident : &mut Self,
            $wname:ident : & mut World
        ) {
            $($body:tt)*
        }
    } => {
        struct $struct_name { $( $field: $fty ),* }

        impl Default for $struct_name {
            fn default() -> Self { Self { $( $field: $default ),* } }
        }

        impl $struct_name {
            #[allow(unused_mut)]
            pub fn into_system(self) -> impl FnMut(&mut $crate::world::World) + Send + 'static {
                let mut __state = self;
                move |$wname: &mut $crate::world::World| {
                    let $slf = &mut __state;
                    $crate::__seq_system_impl! {
                        @mode: closure,
                        @fn_name: dummy,
                        @world: $wname,
                        @before: [],
                        @after: [],
                        @params: [],
                        @body: { $($body)* },
                    }
                }
            }
        }
    };

    // ── Variant B: with state (world + дополнительные параметры) ──
    {
        struct $struct_name:ident {
            $( $field:ident : $fty:ty = $default:expr ),* $(,)?
        }
        fn run(
            $slf:ident : &mut Self,
            $wname:ident : & mut World ,
            $($rest:tt)*
        ) {
            $($body:tt)*
        }
    } => {
        struct $struct_name { $( $field: $fty ),* }

        impl Default for $struct_name {
            fn default() -> Self { Self { $( $field: $default ),* } }
        }

        impl $struct_name {
            #[allow(unused_mut)]
            pub fn into_system(self) -> impl FnMut(&mut $crate::world::World) + Send + 'static {
                let mut __state = self;
                move |$wname: &mut $crate::world::World| {
                    let $slf = &mut __state;
                    $crate::__seq_system_impl! {
                        @mode: closure,
                        @fn_name: dummy,
                        @world: $wname,
                        @before: [],
                        @after: [],
                        @params: [ $($rest)* ],
                        @body: { $($body)* },
                    }
                }
            }
        }
    };
}

// ── Sequential impl helper ───────────────────────────────────────

#[doc(hidden)] #[macro_export]
macro_rules! __seq_system_impl {
    // Base case — fn mode (Variant A)
    {
        @mode: fn, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [], @body: { $( $body:tt )* },
    } => {
        fn $fn_name($world: &mut $crate::world::World) {
            $( $before )*
            $( $body )*
            $( $after )*
        }
    };

    // Base case — closure mode (Variant B: код внутри замыкания)
    {
        @mode: closure, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [], @body: { $( $body:tt )* },
    } => {
        $( $before )*
        $( $body )*
        $( $after )*
    };

    // ═══ With trailing comma ═══

    // world: &mut World — capture name as @world (caller hygiene)
    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut World , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $pname,
        @before: [ $( $before )* ], @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
    }};

    // ctx: Ctx — reborrow &World
    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Ctx , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let $pname: &$crate::world::World = &$world; ],
        @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
    }};

    // WholeWorld — noop for sequential
    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : WholeWorld , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* ], @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
    }};

    // Query tuple
    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ( $( $qty:tt )* ) , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let $pname = $crate::world::CachedQuery::<( $( $qty )* )>::new($world, $crate::component::Tick::ZERO); ],
        @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
    }};

    // Event reader
    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & [ $ev:ty ] , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let $pname = $world.event_reader::<$ev>(); ],
        @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
    }};

    // Event writer
    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut Vec < $ev:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let mut $pname = $world.event_writer::<$ev>(); ],
        @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
    }};

    // Resource write
    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut $ty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let $pname: &mut $ty = $world.resource_mut::<$ty>(); ],
        @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
    }};

    // Resource read
    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & $ty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let $pname: &$ty = $world.resource::<$ty>(); ],
        @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
    }};

    // Commands — user calls cmd.apply(world) manually
    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Cmd , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let mut $pname = $crate::Commands::new(); ],
        @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
    }};

    // Bare type query
    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : $qty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let $pname = $crate::world::CachedQuery::<$qty>::new($world, $crate::component::Tick::ZERO); ],
        @after: [ $( $after )* ],
        @params: [ $( $rest )* ], @body: { $( $body )* },
    }};

    // ═══ Without trailing comma (last param) ═══

    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut World ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $pname,
        @before: [ $( $before )* ], @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
    }};

    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Ctx ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let $pname: &$crate::world::World = &$world; ],
        @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
    }};

    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : WholeWorld ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* ], @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
    }};

    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ( $( $qty:tt )* ) ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let $pname = $crate::world::CachedQuery::<( $( $qty )* )>::new($world, $crate::component::Tick::ZERO); ],
        @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
    }};

    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & [ $ev:ty ] ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let $pname = $world.event_reader::<$ev>(); ],
        @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
    }};

    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut Vec < $ev:ty > ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let mut $pname = $world.event_writer::<$ev>(); ],
        @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
    }};

    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut $ty:ty ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let $pname: &mut $ty = $world.resource_mut::<$ty>(); ],
        @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
    }};

    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & $ty:ty ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let $pname: &$ty = $world.resource::<$ty>(); ],
        @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
    }};

    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Cmd ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let mut $pname = $crate::Commands::new(); ],
        @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
    }};

    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : $qty:ty ],
        @body: { $( $body:tt )* },
    } => { $crate::__seq_system_impl! { @mode: $mode, @fn_name: $fn_name, @world: $world,
        @before: [ $( $before )* let $pname = $crate::world::CachedQuery::<$qty>::new($world, $crate::component::Tick::ZERO); ],
        @after: [ $( $after )* ],
        @params: [], @body: { $( $body )* },
    }};

    // Catch-all
    { @mode: $mode:ident, @fn_name: $fn_name:ident, @world: $world:ident,
        @before: [ $( $before:tt )* ], @after: [ $( $after:tt )* ],
        @params: [ $($rest:tt)+ ], @body: { $( $body:tt )* },
    } => { $crate::__sys_compile_error! { $($rest)* } };
}
