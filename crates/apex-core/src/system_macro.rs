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
            @q: [],
            @r: [],
            @e: [],
            @before: [],
            @after: [],
            @params: [ $($params)* ],
            @body: { $($body)* },
            @struct_body: [],
            @slf: [],
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
        struct $struct_name {
            $( $field: $fty ),*
        }

        impl Default for $struct_name {
            fn default() -> Self {
                Self { $( $field: $default ),* }
            }
        }

        $crate::__system_impl! {
            @fn_name: $struct_name,
            @ctx: ctx,
            @q: [],
            @r: [],
            @e: [],
            @before: [],
            @after: [],
            @params: [ $($params)* ],
            @body: { $($body)* },
            @struct_body: [
                struct $struct_name {
                    $( $field: $fty ),*
                }
            ],
            @slf: [ $slf ],
        }
    };
}

// Helper: emit unit struct (Variant A) or nothing (Variant B, already emitted)
#[doc(hidden)]
#[macro_export]
macro_rules! __emit_struct {
    { [] $fn_name:ident } => {
        #[allow(non_camel_case_types)]
        struct $fn_name;
    };
    { [ $($t:tt)+ ] $fn_name:ident } => {};
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
            - q: (Read<A>, Write<B>)   — component query (tuple)\n  \
            - q: Read<A>               — component query (single)\n  \
            - name: &T                 — resource read\n  \
            - name: &mut T             — resource write\n  \
            - name: &[E]               — event reader\n  \
            - name: &mut Vec<E>        — event writer\n  \
            - cmd: Cmd                 — commands"
        ));
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __system_impl {
    // ── Base case ──
    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__emit_struct! { [ $( $struct_tokens )* ] $fn_name }
        impl $crate::AutoSystem for $fn_name {
            type Query = ( $( $($q)+ ),* );
            type Resources = ( $( $($r)+ ),* );
            type Events = ( $( $($e)+ ),* );
            fn run(&mut self, $ctx: $crate::SystemContext<'_>) {
                $( let $slf_name = &mut *self; )*
                $( $before )*
                $( $body )*
                $( $after )*
            }
        }
    };

    // ═══ With trailing comma ═══

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ( $( $qty:tt )* ) , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ( $( $qty )* ) ],
            @r: [ $( ( $($r)+ ) )* ],
            @e: [ $( ( $($e)+ ) )* ],
            @before: [ $( $before )* let $pname = $ctx.query::<Self::Query>(); ],
            @after: [ $( $after )* ],
            @params: [ $( $rest )* ],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & [ $ev:ty ] , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ],
            @r: [ $( ( $($r)+ ) )* ],
            @e: [ $( ( $($e)+ ) )* ( Listen<$ev> ) ],
            @before: [ $( $before )* let $pname = $ctx.event_reader::<$ev>(); ],
            @after: [ $( $after )* ],
            @params: [ $( $rest )* ],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut Vec < $ev:ty > , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ],
            @r: [ $( ( $($r)+ ) )* ],
            @e: [ $( ( $($e)+ ) )* ( Emit<$ev> ) ],
            @before: [ $( $before )* let mut __system_ev_buf: Vec<$ev> = Vec::new(); let $pname: &mut Vec<$ev> = &mut __system_ev_buf; ],
            @after: [ $( $after )* { let mut __w = $ctx.event_writer::<$ev>(); for __ev in ::std::mem::take(&mut __system_ev_buf) { __w.send(__ev); } } ],
            @params: [ $( $rest )* ],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut $ty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ],
            @r: [ $( ( $($r)+ ) )* ( ResWrite<$ty> ) ],
            @e: [ $( ( $($e)+ ) )* ],
            @before: [ $( $before )* let $pname: &mut $ty = &mut *$ctx.resource_mut::<$ty>(); ],
            @after: [ $( $after )* ],
            @params: [ $( $rest )* ],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & $ty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ],
            @r: [ $( ( $($r)+ ) )* ( ResRead<$ty> ) ],
            @e: [ $( ( $($e)+ ) )* ],
            @before: [ $( $before )* let $pname: &$ty = &*$ctx.resource::<$ty>(); ],
            @after: [ $( $after )* ],
            @params: [ $( $rest )* ],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Cmd , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ],
            @r: [ $( ( $($r)+ ) )* ],
            @e: [ $( ( $($e)+ ) )* ],
            @before: [ $( $before )* let $pname: &mut apex_core::Commands = $ctx.commands(); ],
            @after: [ $( $after )* ],
            @params: [ $( $rest )* ],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : $qty:ty , $( $rest:tt )* ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ( $qty ) ],
            @r: [ $( ( $($r)+ ) )* ],
            @e: [ $( ( $($e)+ ) )* ],
            @before: [ $( $before )* let $pname = $ctx.query::<Self::Query>(); ],
            @after: [ $( $after )* ],
            @params: [ $( $rest )* ],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    // ═══ Without trailing comma ═══

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : ( $( $qty:tt )* ) ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ( $( $qty )* ) ],
            @r: [ $( ( $($r)+ ) )* ],
            @e: [ $( ( $($e)+ ) )* ],
            @before: [ $( $before )* let $pname = $ctx.query::<Self::Query>(); ],
            @after: [ $( $after )* ],
            @params: [],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & [ $ev:ty ] ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ],
            @r: [ $( ( $($r)+ ) )* ],
            @e: [ $( ( $($e)+ ) )* ( Listen<$ev> ) ],
            @before: [ $( $before )* let $pname = $ctx.event_reader::<$ev>(); ],
            @after: [ $( $after )* ],
            @params: [],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut Vec < $ev:ty > ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ],
            @r: [ $( ( $($r)+ ) )* ],
            @e: [ $( ( $($e)+ ) )* ( Emit<$ev> ) ],
            @before: [ $( $before )* let mut __system_ev_buf: Vec<$ev> = Vec::new(); let $pname: &mut Vec<$ev> = &mut __system_ev_buf; ],
            @after: [ $( $after )* { let mut __w = $ctx.event_writer::<$ev>(); for __ev in ::std::mem::take(&mut __system_ev_buf) { __w.send(__ev); } } ],
            @params: [],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & mut $ty:ty ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ],
            @r: [ $( ( $($r)+ ) )* ( ResWrite<$ty> ) ],
            @e: [ $( ( $($e)+ ) )* ],
            @before: [ $( $before )* let $pname: &mut $ty = &mut *$ctx.resource_mut::<$ty>(); ],
            @after: [ $( $after )* ],
            @params: [],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : & $ty:ty ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ],
            @r: [ $( ( $($r)+ ) )* ( ResRead<$ty> ) ],
            @e: [ $( ( $($e)+ ) )* ],
            @before: [ $( $before )* let $pname: &$ty = &*$ctx.resource::<$ty>(); ],
            @after: [ $( $after )* ],
            @params: [],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : Cmd ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ],
            @r: [ $( ( $($r)+ ) )* ],
            @e: [ $( ( $($e)+ ) )* ],
            @before: [ $( $before )* let $pname: &mut apex_core::Commands = $ctx.commands(); ],
            @after: [ $( $after )* ],
            @params: [],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $pname:ident : $qty:ty ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__system_impl! {
            @fn_name: $fn_name,
            @ctx: $ctx,
            @q: [ $( ( $($q)+ ) )* ( $qty ) ],
            @r: [ $( ( $($r)+ ) )* ],
            @e: [ $( ( $($e)+ ) )* ],
            @before: [ $( $before )* let $pname = $ctx.query::<Self::Query>(); ],
            @after: [ $( $after )* ],
            @params: [],
            @body: { $( $body )* },
            @struct_body: [ $( $struct_tokens )* ],
            @slf: [ $( $slf_name )* ],
        }
    };

    // ── Catch-all: unrecognised parameter ──
    {
        @fn_name: $fn_name:ident,
        @ctx: $ctx:ident,
        @q: [ $( ( $($q:tt)+ ) )* ],
        @r: [ $( ( $($r:tt)+ ) )* ],
        @e: [ $( ( $($e:tt)+ ) )* ],
        @before: [ $( $before:tt )* ],
        @after: [ $( $after:tt )* ],
        @params: [ $($rest:tt)+ ],
        @body: { $( $body:tt )* },
        @struct_body: [ $( $struct_tokens:tt )* ],
        @slf: [ $( $slf_name:ident )* ],
    } => {
        $crate::__sys_compile_error! { $($rest)* }
    };
}
