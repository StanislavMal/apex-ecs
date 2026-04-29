#include "flecs_helper.h"

ecs_entity_t helper_register_component(ecs_world_t *world, size_t size, size_t alignment) {
    ecs_component_desc_t desc = {0};
    desc.type.size = (ecs_size_t)size;
    desc.type.alignment = (ecs_size_t)alignment;
    return ecs_component_init(world, &desc);
}

ecs_query_t* helper_query_create(
    ecs_world_t *world,
    const ecs_id_t *components,
    const int16_t *inout_flags,
    int32_t term_count)
{
    ecs_query_desc_t desc = {0};
    for (int32_t i = 0; i < term_count && i < FLECS_TERM_COUNT_MAX; i++) {
        desc.terms[i].id = components[i];
        desc.terms[i].inout = inout_flags[i];
        desc.terms[i].field_index = (int8_t)i;
    }
    return ecs_query_init(world, &desc);
}

void* helper_iter_field(const ecs_iter_t *it, int8_t field, size_t size) {
    return ecs_field_w_size(it, size, field);
}

void helper_query_iter(ecs_world_t *world, ecs_query_t *query, ecs_iter_t *out_it) {
    *out_it = ecs_query_iter(world, query);
}

size_t helper_iter_sizeof(void) {
    return sizeof(ecs_iter_t);
}

int32_t helper_iter_count(const ecs_iter_t *it) {
    return it->count;
}
