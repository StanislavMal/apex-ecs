use super::ffi;
use std::alloc::{alloc, dealloc, Layout};

pub struct FlecsWorld {
    raw: *mut std::ffi::c_void,
}

unsafe impl Send for FlecsWorld {}
unsafe impl Sync for FlecsWorld {}

impl FlecsWorld {
    pub fn new() -> Self {
        let raw = unsafe { ffi::ecs_init() };
        assert!(!raw.is_null(), "ecs_init failed");
        FlecsWorld { raw }
    }

    pub fn raw(&self) -> *mut std::ffi::c_void {
        self.raw
    }

    pub fn register_component<T>(&mut self) -> ffi::ecs_entity_t {
        let size = std::mem::size_of::<T>();
        let align = std::mem::align_of::<T>();
        let id = unsafe { ffi::helper_register_component(self.raw, size, align) };
        assert!(id != 0, "helper_register_component failed");
        id
    }

    pub fn register_component_raw(
        &mut self,
        size: usize,
        align: usize,
    ) -> ffi::ecs_entity_t {
        let id = unsafe { ffi::helper_register_component(self.raw, size, align) };
        assert!(id != 0, "helper_register_component failed");
        id
    }

    pub fn add_id(&mut self, entity: ffi::ecs_entity_t, component: ffi::ecs_id_t) {
        unsafe { ffi::ecs_add_id(self.raw, entity, component) }
    }

    pub fn remove_id(&mut self, entity: ffi::ecs_entity_t, component: ffi::ecs_id_t) {
        unsafe { ffi::ecs_remove_id(self.raw, entity, component) }
    }

    pub fn set_id_raw(
        &mut self,
        entity: ffi::ecs_entity_t,
        component: ffi::ecs_id_t,
        ptr: *const std::ffi::c_void,
        size: usize,
    ) {
        unsafe { ffi::ecs_set_id(self.raw, entity, component, size, ptr) }
    }
}

impl Drop for FlecsWorld {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { ffi::ecs_fini(self.raw) };
        }
    }
}

pub struct FlecsFilter {
    raw: *mut std::ffi::c_void,
}

impl FlecsFilter {
    pub fn new(world: &FlecsWorld, term_ids: &[ffi::ecs_id_t], inout_flags: &[i16]) -> Self {
        assert_eq!(term_ids.len(), inout_flags.len());
        let raw = unsafe {
            ffi::helper_query_create(
                world.raw(),
                term_ids.as_ptr(),
                inout_flags.as_ptr(),
                term_ids.len() as i32,
            )
        };
        assert!(!raw.is_null(), "helper_query_create failed");
        FlecsFilter { raw }
    }

    pub fn iter(&self, world: &FlecsWorld) -> FlecsIter {
        let iter_size = unsafe { ffi::helper_iter_sizeof() };
        let layout = Layout::from_size_align(iter_size, 8).unwrap();
        let buf = unsafe { alloc(layout) };
        unsafe {
            ffi::helper_query_iter(
                world.raw(),
                self.raw,
                buf as *mut ffi::ecs_iter_t,
            );
        }
        FlecsIter {
            buf,
            layout,
        }
    }
}

impl Drop for FlecsFilter {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { ffi::ecs_query_fini(self.raw) };
        }
    }
}

pub struct FlecsIter {
    buf: *mut u8,
    layout: Layout,
}

impl FlecsIter {
    pub fn next(&mut self) -> bool {
        unsafe {
            ffi::ecs_query_next(self.buf as *mut ffi::ecs_iter_t)
        }
    }

    pub fn count(&self) -> i32 {
        unsafe { ffi::helper_iter_count(self.buf as *const ffi::ecs_iter_t) }
    }

    pub fn field<T>(&self, index: i8) -> Option<&[T]> {
        unsafe {
            let ptr = ffi::helper_iter_field(
                self.buf as *const ffi::ecs_iter_t,
                index,
                std::mem::size_of::<T>(),
            );
            if ptr.is_null() {
                None
            } else {
                Some(std::slice::from_raw_parts(ptr as *const T, self.count() as usize))
            }
        }
    }

    pub fn field_mut<T>(&self, index: i8) -> Option<&mut [T]> {
        unsafe {
            let ptr = ffi::helper_iter_field(
                self.buf as *const ffi::ecs_iter_t,
                index,
                std::mem::size_of::<T>(),
            );
            if ptr.is_null() {
                None
            } else {
                Some(std::slice::from_raw_parts_mut(ptr as *mut T, self.count() as usize))
            }
        }
    }
}

impl Drop for FlecsIter {
    fn drop(&mut self) {
        if !self.buf.is_null() {
            unsafe { dealloc(self.buf, self.layout) };
        }
    }
}
