#![cfg(feature = "flecs")]

#[test]
fn test_ecs_init_fini() {
    unsafe {
        let world = apex_bench::flecs::ffi::ecs_init();
        assert!(!world.is_null());
        apex_bench::flecs::ffi::ecs_fini(world);
    }
}

#[test]
fn test_register_component() {
    unsafe {
        let world = apex_bench::flecs::ffi::ecs_init();
        let id = apex_bench::flecs::ffi::helper_register_component(world, 4, 4);
        assert!(id != 0);
        apex_bench::flecs::ffi::ecs_fini(world);
    }
}

#[test]
fn test_create_entity_with_tag() {
    unsafe {
        let world = apex_bench::flecs::ffi::ecs_init();
        let tag = apex_bench::flecs::ffi::helper_register_component(world, 1, 1);
        let e = apex_bench::flecs::ffi::ecs_new_w_id(world, tag);
        assert!(e != 0);
        apex_bench::flecs::ffi::ecs_fini(world);
    }
}

#[test]
fn test_query_create() {
    unsafe {
        let world = apex_bench::flecs::ffi::ecs_init();
        let comp = apex_bench::flecs::ffi::helper_register_component(world, 4, 4);
        let components = [comp];
        let inout = [apex_bench::flecs::ffi::EcsInOutIn];
        let query = apex_bench::flecs::ffi::helper_query_create(
            world, components.as_ptr(), inout.as_ptr(), 1);
        assert!(!query.is_null());
        apex_bench::flecs::ffi::ecs_query_fini(query);
        apex_bench::flecs::ffi::ecs_fini(world);
    }
}

#[test]
fn test_query_iter_size() {
    let size = unsafe { apex_bench::flecs::ffi::helper_iter_sizeof() };
    println!("ecs_iter_t size = {}", size);
    assert!(size > 0);
    assert!(size <= 4096, "ecs_iter_t is too large: {}", size);
}

#[test]
fn test_query_simple_iter() {
    unsafe {
        let world = apex_bench::flecs::ffi::ecs_init();
        let pos = apex_bench::flecs::ffi::helper_register_component(world, 12, 4);
        let vel = apex_bench::flecs::ffi::helper_register_component(world, 12, 4);

        // Create 100 entities with Position and Velocity
        for _ in 0..100 {
            let e = apex_bench::flecs::ffi::ecs_new_w_id(world, pos);
            apex_bench::flecs::ffi::ecs_add_id(world, e, vel);
        }

        // Create query
        let components = [vel, pos];
        let inout = [apex_bench::flecs::ffi::EcsInOutIn, apex_bench::flecs::ffi::EcsInOutOut];
        let query = apex_bench::flecs::ffi::helper_query_create(
            world, components.as_ptr(), inout.as_ptr(), 2);
        assert!(!query.is_null());

        // Iterate
        let iter_size = apex_bench::flecs::ffi::helper_iter_sizeof();
        let mut buf = vec![0u8; iter_size];
        apex_bench::flecs::ffi::helper_query_iter(
            world, query, buf.as_mut_ptr() as *mut apex_bench::flecs::ffi::ecs_iter_t);
        let mut count = 0i32;
        while apex_bench::flecs::ffi::ecs_query_next(
            buf.as_mut_ptr() as *mut apex_bench::flecs::ffi::ecs_iter_t)
        {
            let c = apex_bench::flecs::ffi::helper_iter_count(
                buf.as_ptr() as *const apex_bench::flecs::ffi::ecs_iter_t);
            count += c;
        }
        assert_eq!(count, 100, "Expected 100 entities, got {}", count);
        apex_bench::flecs::ffi::ecs_query_fini(query);
        apex_bench::flecs::ffi::ecs_fini(world);
    }
}

#[test]
fn test_create_entity_with_component() {
    unsafe {
        let world = apex_bench::flecs::ffi::ecs_init();
        let comp = apex_bench::flecs::ffi::helper_register_component(world, 4, 4);
        assert!(comp != 0);
        let e = apex_bench::flecs::ffi::ecs_new_w_id(world, comp);
        assert!(e != 0);
        let val: f32 = 42.0;
        apex_bench::flecs::ffi::ecs_set_id(
            world, e, comp, 4,
            &val as *const _ as *const std::ffi::c_void,
        );
        apex_bench::flecs::ffi::ecs_fini(world);
    }
}
