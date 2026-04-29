#![allow(non_camel_case_types, dead_code)]

pub type ecs_id_t = u64;
pub type ecs_entity_t = ecs_id_t;
pub type ecs_iter_t = [u64; 64];

pub const EcsInOutDefault: i16 = 0;
pub const EcsInOutNone: i16 = 1;
pub const EcsInOutIn: i16 = 2;
pub const EcsInOutOut: i16 = 3;
pub const EcsInOutInOut: i16 = 4;
pub const EcsInOutFilter: i16 = 5;
pub const EcsAnd: i16 = 1;

extern "C" {
    pub fn ecs_init() -> *mut std::ffi::c_void;
    pub fn ecs_fini(world: *mut std::ffi::c_void) -> i32;

    pub fn ecs_new_w_id(
        world: *mut std::ffi::c_void,
        component: ecs_id_t,
    ) -> ecs_entity_t;

    pub fn ecs_add_id(
        world: *mut std::ffi::c_void,
        entity: ecs_entity_t,
        component: ecs_id_t,
    );

    pub fn ecs_remove_id(
        world: *mut std::ffi::c_void,
        entity: ecs_entity_t,
        component: ecs_id_t,
    );

    pub fn ecs_set_id(
        world: *mut std::ffi::c_void,
        entity: ecs_entity_t,
        component: ecs_id_t,
        size: usize,
        ptr: *const std::ffi::c_void,
    );

    pub fn ecs_query_fini(query: *mut std::ffi::c_void);

    pub fn ecs_query_next(it: *mut ecs_iter_t) -> bool;

    /* Helper functions (from flecs_helper.c) */
    pub fn helper_register_component(
        world: *mut std::ffi::c_void,
        size: usize,
        alignment: usize,
    ) -> ecs_entity_t;

    pub fn helper_query_create(
        world: *mut std::ffi::c_void,
        components: *const ecs_id_t,
        inout_flags: *const i16,
        term_count: i32,
    ) -> *mut std::ffi::c_void;

    pub fn helper_iter_field(
        it: *const ecs_iter_t,
        field: i8,
        size: usize,
    ) -> *mut std::ffi::c_void;

    /* Note: ecs_query_iter returns an ecs_iter_t by value.
       We call it from C instead to avoid struct return issues. */
    pub fn helper_query_iter(
        world: *mut std::ffi::c_void,
        query: *mut std::ffi::c_void,
        out_it: *mut ecs_iter_t,
    );

    pub fn helper_iter_sizeof() -> usize;
    pub fn helper_iter_count(it: *const ecs_iter_t) -> i32;
}
