#ifndef FLECS_HELPER_H
#define FLECS_HELPER_H

#include "flecs.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Simplified component registration */
ecs_entity_t helper_register_component(ecs_world_t *world, size_t size, size_t alignment);

/* Create a query from parallel arrays of component IDs, inout flags, and term count */
ecs_query_t* helper_query_create(
    ecs_world_t *world,
    const ecs_id_t *components,
    const int16_t *inout_flags,
    int32_t term_count);

/* Get field pointer and count from iterator */
void* helper_iter_field(const ecs_iter_t *it, int8_t field, size_t size);

/* Get iterator via pointer (avoids struct return issues across FFI) */
void helper_query_iter(ecs_world_t *world, ecs_query_t *query, ecs_iter_t *out_it);

/* Returns sizeof(ecs_iter_t) so Rust can allocate the right buffer */
size_t helper_iter_sizeof(void);

/* Get count from an iterator */
int32_t helper_iter_count(const ecs_iter_t *it);

#ifdef __cplusplus
}
#endif

#endif
