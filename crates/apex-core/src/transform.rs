//! TransformPropagation — hierarchical transforms.
//!
//! # Architecture
//!
//! - [`LocalTransform`] — entity position/rotation/scale (local space)
//! - [`GlobalTransform`] — resulting world matrix (recomputed from the hierarchy)
//! - [`propagate_transforms`] — exclusive system performing the hierarchical recompute
//!
//! # DX (after C1/C2)
//!
//! Manual `TransformDirty` is **removed**: dirty-detection goes through
//! `Changed<LocalTransform>` — reliable for mutations both via `Query<Write>` and
//! via `World::get_mut` (C1). Just change `LocalTransform` and the recompute
//! happens automatically, cascading to descendants. Just spawn an
//! entity with a single `LocalTransform` — `GlobalTransform` is created by the
//! **`propagate_transforms` system on its first pass** (not at spawn time; until then
//! `get::<GlobalTransform>` returns `None`). See the doc on [`GlobalTransform`].
//!
//! # Algorithm
//!
//! 1. Collect entities with `Changed<LocalTransform>` (since the previous run).
//! 2. Keep the "dirty-roots" — changed entities without a changed ancestor
//!    (the rest lie inside their subtrees and will be recomputed by the descent).
//! 3. For each dirty-root, descend the whole subtree (DFS), passing the
//!    parent world matrix by value: `Global = parent_global * Local`.
//!    Each node is visited exactly once; there are no stale parent reads by
//!    construction (the parent is always computed before the child).
//!
//! # Usage in the Scheduler
//!
//! ```ignore
//! use apex_core::transform::{LocalTransform, GlobalTransform, TransformPlugin};
//! use apex_scheduler::stage::StageLabel;
//!
//! TransformPlugin::register_components(&mut world);
//!
//! scheduler.add_system_to_stage(
//!     "propagate_transforms",
//!     apex_core::transform::propagate_transforms,
//!     StageLabel::PostUpdate,
//! );
//! ```

use glam::{DVec3, Mat3, Mat4, Quat, Vec3};

use crate::{component::Tick, entity::Entity, relations::ChildOf, world::World};

/// Generational "visit map" keyed by `entity.index`: O(1) mark/contains without
/// hashing and without per-frame clearing (generation comparison; reset on wrap).
/// 21k FxHashSet hash-inserts per frame cost more than the rest of propagate combined.
///
/// Public utility (CR-M4) for the "set of entities per frame" pattern — a replacement
/// for per-frame `FxHashSet<Entity>` in consumers (engine extract: shadow_markers,
/// skins active-set). The key is `entity.index()`; the entity generation is not involved,
/// so it is correct only for short-lived (per-frame) sets.
#[derive(Default)]
pub struct IndexStamp {
    stamps: Vec<u32>,
    generation: u32,
}

impl IndexStamp {
    /// Start a new generation (previous marks are instantly "forgotten").
    pub fn next_generation(&mut self) {
        let (g, wrapped) = self.generation.overflowing_add(1);
        self.generation = g;
        if wrapped || g == 0 {
            // Once every 2^32 frames: generation zero collides with the cells' default — clear them.
            self.stamps.iter_mut().for_each(|s| *s = u32::MAX);
            self.generation = 1;
        }
    }

    /// Mark the index in the current generation.
    #[inline]
    pub fn mark(&mut self, index: u32) {
        let i = index as usize;
        if i >= self.stamps.len() {
            self.stamps.resize(i + 1, self.generation.wrapping_sub(1));
        }
        self.stamps[i] = self.generation;
    }

    /// Whether the index is marked in the current generation.
    #[inline]
    pub fn contains(&self, index: u32) -> bool {
        self.stamps.get(index as usize) == Some(&self.generation)
    }
}

// ── Transform components ─────────────────────────────────────

/// Local transform of an entity (relative to the parent).
///
/// If the entity has no parent (no ChildOf) — this is its world transform.
///
/// # Precision model (large worlds, engine ADR-056 / TD-203)
///
/// `translation` is **f64**: the representable step of an f32 coordinate is
/// `|v|·2⁻²³` (a millimetre-true world ends at ~±16 km), and the loss happens
/// at the WRITE — a 1 cm step authored a thousand km out is swallowed before
/// any system sees it. Rotation and scale stay f32: their precision does not
/// decay with distance from the origin. This is the UE5 LWC split.
#[derive(Debug, Clone, Copy, PartialEq, apex_macros::Component)]
pub struct LocalTransform {
    pub translation: DVec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl LocalTransform {
    /// Identity transform (zero translation, identity rotation, unit scale).
    pub const IDENTITY: Self = Self {
        translation: DVec3::ZERO,
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    };

    pub fn from_translation(t: DVec3) -> Self {
        Self {
            translation: t,
            ..Self::IDENTITY
        }
    }

    /// Transform from coordinates (the most frequent constructor, 1:1 Bevy
    /// `Transform::from_xyz` — but f64, see the type's precision model).
    #[inline]
    pub fn from_xyz(x: f64, y: f64, z: f64) -> Self {
        Self::from_translation(DVec3::new(x, y, z))
    }

    /// `from_translation` for an f32 vector (glTF node locals, math results
    /// that are f32 anyway). Exact: f32 → f64 widening is lossless.
    #[inline]
    pub fn from_translation_f32(t: Vec3) -> Self {
        Self::from_translation(t.as_dvec3())
    }

    pub fn from_rotation(r: Quat) -> Self {
        Self {
            rotation: r,
            ..Self::IDENTITY
        }
    }

    pub fn from_scale(s: Vec3) -> Self {
        Self {
            scale: s,
            ..Self::IDENTITY
        }
    }

    // ── Builders (1:1 Bevy Transform) ────────────────────────────

    /// Replace translation (builder).
    #[inline]
    #[must_use]
    pub fn with_translation(mut self, translation: DVec3) -> Self {
        self.translation = translation;
        self
    }

    /// Replace rotation (builder).
    #[inline]
    #[must_use]
    pub fn with_rotation(mut self, rotation: Quat) -> Self {
        self.rotation = rotation;
        self
    }

    /// Replace scale (builder).
    #[inline]
    #[must_use]
    pub fn with_scale(mut self, scale: Vec3) -> Self {
        self.scale = scale;
        self
    }

    /// Rotate so the local forward (−Z) points at `target`,
    /// and the local +Y is aligned to `up` without roll (1:1 Bevy
    /// `Transform::looking_at`).
    ///
    /// ⚠ This is NOT `Quat::from_rotation_arc` — that leaves an arbitrary roll
    /// (the horizon tilts).
    #[inline]
    #[must_use]
    pub fn looking_at(self, target: DVec3, up: Vec3) -> Self {
        // The difference of two far points is small — narrow AFTER subtracting.
        self.looking_to((target - self.translation).as_vec3(), up)
    }

    /// Rotate so the local forward (−Z) points along `direction`
    /// (1:1 Bevy `Transform::looking_to`). See [`Self::looking_at`].
    #[inline]
    #[must_use]
    pub fn looking_to(mut self, direction: Vec3, up: Vec3) -> Self {
        self.rotation = look_to_rotation(direction, up);
        self
    }

    // ── Directions (world axes of the local basis) ──────────────

    /// Local forward: −Z in world coordinates.
    #[inline]
    pub fn forward(&self) -> Vec3 {
        self.rotation * Vec3::NEG_Z
    }

    /// Local back: +Z in world coordinates.
    #[inline]
    pub fn back(&self) -> Vec3 {
        self.rotation * Vec3::Z
    }

    /// Local right: +X in world coordinates.
    #[inline]
    pub fn right(&self) -> Vec3 {
        self.rotation * Vec3::X
    }

    /// Local left: −X in world coordinates.
    #[inline]
    pub fn left(&self) -> Vec3 {
        self.rotation * Vec3::NEG_X
    }

    /// Local up: +Y in world coordinates.
    #[inline]
    pub fn up(&self) -> Vec3 {
        self.rotation * Vec3::Y
    }

    /// Local down: −Y in world coordinates.
    #[inline]
    pub fn down(&self) -> Vec3 {
        self.rotation * Vec3::NEG_Y
    }

    /// Convert into a 4x4 affine matrix. **Lossy for far-out translations**
    /// (f64 → f32): fine for local-space math (bone chains, node templates,
    /// near-origin fixtures); the propagation itself never uses it — the
    /// translation column stays f64 end to end.
    #[inline]
    pub fn to_matrix(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(
            self.scale,
            self.rotation,
            self.translation.as_vec3(),
        )
    }

    /// The linear (rotation·scale) part — bit-identical to the upper-left 3×3
    /// of [`Self::to_matrix`] (same glam op sequence: quat basis, columns
    /// scaled), which keeps propagation results bit-stable vs the old Mat4
    /// composition.
    #[inline]
    pub fn linear_m3(&self) -> Mat3 {
        let r = Mat3::from_quat(self.rotation);
        Mat3::from_cols(
            r.x_axis * self.scale.x,
            r.y_axis * self.scale.y,
            r.z_axis * self.scale.z,
        )
    }
}

/// Quaternion "look along `direction` with `up` without roll" — the canonical
/// look-rotation (1:1 Bevy `Transform::looking_to`): back = −direction,
/// right = up × back, up' = back × right. Degenerate inputs (zero/NaN
/// direction, up ∥ direction) fall back safely as in Bevy
/// (`any_orthonormal_vector`).
fn look_to_rotation(direction: Vec3, up: Vec3) -> Quat {
    let back = -direction.try_normalize().unwrap_or(Vec3::NEG_Z);
    let up = up.try_normalize().unwrap_or(Vec3::Y);
    let right = up
        .cross(back)
        .try_normalize()
        .unwrap_or_else(|| up.any_orthonormal_vector());
    let up = back.cross(right);
    Quat::from_mat3(&Mat3::from_cols(right, up, back))
}

impl Default for LocalTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Global (world) transform of an entity.
///
/// # When it appears (important!)
///
/// `GlobalTransform` is **NOT added at spawn time** — just spawn an
/// entity with a single [`LocalTransform`]. The component is created **automatically** by the
/// [`propagate_transforms`] system on its **first pass** after the spawn (for entities that
/// have a `LocalTransform` but not yet a `GlobalTransform`).
///
/// In practice: between `world.spawn((LocalTransform...,))` and the first run of
/// `propagate_transforms` (PostUpdate), `world.get::<GlobalTransform>(e)` returns
/// `None`. After the first pass — `Some(..)` with a correct matrix. Rendering and
/// other consumers read `GlobalTransform` after propagate within the same frame.
///
/// If `GlobalTransform` is needed **immediately** at spawn — add it to the bundle
/// explicitly: `world.spawn((LocalTransform::from_translation(t), GlobalTransform::IDENTITY))`
/// (its value will be recomputed by propagate anyway).
///
/// Recomputed in PostUpdate by the `propagate_transforms` system.
/// Not serialized — reconstructed from the hierarchy + LocalTransform.
///
/// # Representation (engine ADR-056 hybrid, ratified 2026-07-30)
///
/// The linear part (`m3`: rotation·scale·shear — the full upper-left 3×3 of
/// the old `Mat4`, so non-uniform-scaled hierarchies keep their shear) stays
/// **f32**; the translation is **f64**. Same 64 bytes as the old `Mat4`
/// newtype; orientation precision does not decay with distance, translation
/// precision no longer decays either. A full `DMat4` would double both the
/// memory and the propagation cost for no measurable gain.
///
/// Consumers narrow through ONE of two funnels:
/// - [`Self::rebased_mat4`] — exact narrowing into a render/anchor window
///   (subtract the f64 origin FIRST); the engine's extract boundary.
/// - [`Self::to_mat4_lossy`] — the absolute matrix in f32, i.e. exactly the
///   pre-C2 behavior; fine near the origin, quantizes far out (`|v|·2⁻²³`).
#[derive(Debug, Clone, Copy, PartialEq, apex_macros::Component)]
pub struct GlobalTransform {
    /// Rotation·scale·shear — the full linear part, f32.
    pub m3: Mat3,
    /// World translation, f64 (the authoritative half; engine TD-203).
    pub translation: DVec3,
}

impl GlobalTransform {
    pub const IDENTITY: Self = Self {
        m3: Mat3::IDENTITY,
        translation: DVec3::ZERO,
    };

    /// World transform from coordinates (for spawning root entities
    /// that need the matrix immediately — before the first propagate).
    #[inline]
    pub fn from_xyz(x: f64, y: f64, z: f64) -> Self {
        Self {
            m3: Mat3::IDENTITY,
            translation: DVec3::new(x, y, z),
        }
    }

    /// Exact widening from an affine f32 matrix (fixtures, glTF spawn
    /// matrices, math results that were f32 anyway). f32 → f64 is lossless.
    #[inline]
    pub fn from_mat4(m: Mat4) -> Self {
        Self {
            m3: Mat3::from_mat4(m),
            translation: m.w_axis.truncate().as_dvec3(),
        }
    }

    /// World transform "from `eye`, looking at `target`" (the same
    /// roll-free look-rotation as [`LocalTransform::looking_at`]).
    /// Typical case — spawning a light/camera by matrix.
    #[inline]
    pub fn looking_at(eye: DVec3, target: DVec3, up: Vec3) -> Self {
        Self {
            m3: Mat3::from_quat(look_to_rotation((target - eye).as_vec3(), up)),
            translation: eye,
        }
    }

    /// The absolute world matrix, narrowed to f32 — **exactly the pre-C2
    /// value** (the old `Mat4` newtype), including its far-from-origin
    /// quantization. For near-origin/main-thread math that tolerated f32
    /// before; anything crossing to the GPU goes through [`Self::rebased_mat4`].
    #[inline]
    pub fn to_mat4_lossy(&self) -> Mat4 {
        mat4_from_m3_t(self.m3, self.translation.as_vec3())
    }

    /// The window-relative world matrix: the translation is rebased against
    /// `origin` in f64 BEFORE narrowing, the 3×3 passes through bit-for-bit.
    /// This is the single exact f64 → f32 funnel (the engine's extract
    /// boundary; ADR-056 camera-relative rendering).
    #[inline]
    pub fn rebased_mat4(&self, origin: DVec3) -> Mat4 {
        mat4_from_m3_t(self.m3, (self.translation - origin).as_vec3())
    }

    /// World forward (−Z), 1:1 Bevy `GlobalTransform::forward`. See [`TransformDirections`].
    #[inline]
    pub fn forward(&self) -> Vec3 {
        (-self.m3.z_axis).normalize_or_zero()
    }
    /// World back (+Z), 1:1 Bevy `GlobalTransform::back`. See [`TransformDirections`].
    #[inline]
    pub fn back(&self) -> Vec3 {
        self.m3.z_axis.normalize_or_zero()
    }
    /// World right (+X), 1:1 Bevy `GlobalTransform::right`.
    #[inline]
    pub fn right(&self) -> Vec3 {
        self.m3.x_axis.normalize_or_zero()
    }
    /// World left (−X), 1:1 Bevy `GlobalTransform::left`.
    #[inline]
    pub fn left(&self) -> Vec3 {
        (-self.m3.x_axis).normalize_or_zero()
    }
    /// World up (+Y), 1:1 Bevy `GlobalTransform::up`.
    #[inline]
    pub fn up(&self) -> Vec3 {
        self.m3.y_axis.normalize_or_zero()
    }
    /// World down (−Y), 1:1 Bevy `GlobalTransform::down`.
    #[inline]
    pub fn down(&self) -> Vec3 {
        (-self.m3.y_axis).normalize_or_zero()
    }

    /// Compose with a child's local transform: `world = self ∘ local`.
    /// The linear parts multiply in f32 (bit-identical to the old Mat4×Mat4
    /// 3×3 block); the translation accumulates in f64 — the local offset is
    /// rotated/scaled through the parent's linear part widened to f64, so a
    /// kilometre-scale child offset loses nothing either.
    #[inline]
    pub fn mul_local(&self, local: &LocalTransform) -> Self {
        Self {
            m3: self.m3 * local.linear_m3(),
            translation: self.translation + self.m3.as_dmat3() * local.translation,
        }
    }
}

/// Affine 4×4 from a linear 3×3 + translation (glam has no such constructor
/// for `Mat4`; column extension is exact — no arithmetic involved).
#[inline]
fn mat4_from_m3_t(m3: Mat3, t: Vec3) -> Mat4 {
    Mat4::from_cols(
        m3.x_axis.extend(0.0),
        m3.y_axis.extend(0.0),
        m3.z_axis.extend(0.0),
        t.extend(1.0),
    )
}

/// Semantic accessors for world directions of a transform matrix (local→world),
/// 1:1 Bevy `Transform`/`GlobalTransform`: **forward = local −Z**, back = +Z, right = +X,
/// left = −X, up = +Y, down = −Y (each normalized).
///
/// **Use these instead of "raw" `±matrix.z_axis`/`x_axis`/`y_axis`.** Raw column access
/// is easy to get sign-wrong: for example, the view-matrix basis of a spot-light must be built from
/// `back()` (+Z), not from forward (−Z) — a sign mix-up mirrored the spotlight's shadow map
/// (it self-shadowed its own volumetric cone). Named directions make the sign impossible
/// to confuse (see `apex-engine/plans/TECH_DEBT.md`, fix 2026-06-15).
pub trait TransformDirections {
    /// Local −Z in world space (where the object "looks").
    fn forward(&self) -> Vec3;
    /// Local +Z in world space.
    fn back(&self) -> Vec3;
    /// Local +X in world space.
    fn right(&self) -> Vec3;
    /// Local −X in world space.
    fn left(&self) -> Vec3;
    /// Local +Y in world space.
    fn up(&self) -> Vec3;
    /// Local −Y in world space.
    fn down(&self) -> Vec3;
}

impl TransformDirections for Mat4 {
    #[inline]
    fn forward(&self) -> Vec3 {
        (-self.z_axis.truncate()).normalize_or_zero()
    }
    #[inline]
    fn back(&self) -> Vec3 {
        self.z_axis.truncate().normalize_or_zero()
    }
    #[inline]
    fn right(&self) -> Vec3 {
        self.x_axis.truncate().normalize_or_zero()
    }
    #[inline]
    fn left(&self) -> Vec3 {
        (-self.x_axis.truncate()).normalize_or_zero()
    }
    #[inline]
    fn up(&self) -> Vec3 {
        self.y_axis.truncate().normalize_or_zero()
    }
    #[inline]
    fn down(&self) -> Vec3 {
        (-self.y_axis.truncate()).normalize_or_zero()
    }
}

impl From<&LocalTransform> for GlobalTransform {
    /// World transform of a root entity == its local TRS (no parent).
    #[inline]
    fn from(local: &LocalTransform) -> Self {
        Self {
            m3: local.linear_m3(),
            translation: local.translation,
        }
    }
}

impl Default for GlobalTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

// ── Propagation system ─────────────────────────────────────────

/// Scratch buffers + change-detection state for [`propagate_transforms`].
/// Reused every frame, avoiding Vec allocations on the hot path.
#[derive(Default)]
pub struct TransformScratch {
    /// Tick of the previous propagate run — base for `Changed<LocalTransform>`.
    pub(crate) last_run: Tick,
    /// List of dirty entities from the query (step 1)
    pub(crate) dirty_entities: Vec<Entity>,
    /// O(1) dirty check by entity.index (generational stamp instead of a hash-set)
    pub(crate) dirty: IndexStamp,
    /// DFS descent stack over subtrees: (entity, PARENT world transform).
    pub(crate) stack: Vec<(Entity, GlobalTransform)>,
}

/// Exclusive system: recomputes `GlobalTransform` for every entity
/// whose `LocalTransform` **changed** since the previous run (`Changed<LocalTransform>`),
/// and all of their descendants. Manual `TransformDirty` is no longer needed — after C1,
/// `Changed<LocalTransform>` is reliable on all mutation paths (`Query<Write>` and
/// `World::get_mut`). Runs in PostUpdate.
///
/// # Algorithm (subtrees from dirty-roots)
///
/// 1. Collect entities with `Changed<LocalTransform>` (since the previous `last_run`).
/// 2. Keep the **dirty-roots** — changed entities that have NO changed
///    ancestor (the rest are inside their subtrees). The walk-up stops at the first
///    dirty ancestor, so in typical scenes this is 1–2 lookups per entity.
/// 3. From each dirty-root — an iterative DFS over the whole subtree, passing the
///    parent world matrix **by value**: `Global = parent_global * Local`;
///    if a `GlobalTransform` does not exist yet — auto-initialize it (DX: spawn with a single
///    `LocalTransform` is enough). Each node is visited exactly once and
///    strictly after its parent — there are no stale reads of the parent matrix by construction
///    (the previous version recomputed dirty nodes under a clean intermediate ancestor
///    twice: first with the old matrix, then again by cascade).
///
/// Cost — O(|union of dirty subtrees|): a static scene with a single
/// mover visits only its subtree; a fully animated one — every node
/// once. The `ChildOf` hierarchy is assumed acyclic (as everywhere in the core).
///
/// # Change-detection
///
/// The base is `scratch.last_run`; at the end `world.current_tick()` is written. Requires
/// per-frame tick advancement (`world.tick()` before `run()`; automatic in C7).
///
/// # Diagnostics
///
/// `APEX_PROP_TRACE=1` — logs the phases (changed-query / descent, number of dirty/visits).
///
/// # Resources
///
/// Uses [`TransformScratch`] to reuse buffers between frames.
///
/// Defensive bound on the ChildOf ancestor walk. `add_relation` rejects
/// cycle-forming edges, so a real hierarchy never approaches this; it only stops
/// a pathological/corrupt cycle from hanging the frame.
const MAX_PROPAGATE_DEPTH: usize = 1 << 20;

pub fn propagate_transforms(world: &mut World) {
    // Take the scratch buffer out of the resources (or create a new one on the first call).
    // remove_resource moves the value into a local variable, releasing
    // the borrow of world — this lets us call world.get()/world.insert()
    // without a borrow-checker conflict.
    let mut scratch = world
        .remove_resource::<TransformScratch>()
        .unwrap_or_default();

    let last_run = scratch.last_run;
    let this_run = world.current_tick();

    // Clear all buffers (capacity is preserved — allocations are reused)
    scratch.dirty_entities.clear();
    scratch.dirty.next_generation();
    scratch.stack.clear();

    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let trace = *TRACE.get_or_init(|| std::env::var("APEX_PROP_TRACE").is_ok_and(|v| v == "1"));
    let t0 = std::time::Instant::now();

    // 1. Collect entities with a changed LocalTransform (since the previous run) and mark them in
    //    the stamp map. The semantics are exactly `Query<Changed<LocalTransform>>` (the same
    //    `is_newer_than`), but via a direct linear scan of the archetypes' tick columns: generic
    //    query iteration cost ~35ns/row (fetch_item indirection + closure),
    //    here — a tight loop over `Vec<Tick>` (parity pinned by the test
    //    `direct_tick_scan_matches_changed_query`).
    {
        let TransformScratch {
            dirty_entities,
            dirty,
            ..
        } = &mut scratch;
        if let Some(lt_id) = world.registry.get_id::<LocalTransform>() {
            for arch in &world.archetypes {
                let Some(col_idx) = arch.column_index(lt_id) else {
                    continue;
                };
                let col = &arch.columns[col_idx];
                for (tick, &entity) in col.change_ticks.iter().zip(arch.entities.iter()) {
                    if tick.get().is_newer_than(last_run) {
                        dirty_entities.push(entity);
                        dirty.mark(entity.index);
                    }
                }
            }
        }
    }
    let t1 = std::time::Instant::now();

    if scratch.dirty_entities.is_empty() {
        // Nothing changed — commit the tick and return.
        scratch.last_run = this_run;
        world.insert_resource(scratch);
        return;
    }

    // 2. Seed the stack with dirty-roots: dirty entities without a dirty ancestor. The walk-up over
    //    ancestors stops at the first dirty one (then the entity is inside its subtree and will be
    //    recomputed by the descent — it must not be seeded separately, otherwise double-process).
    //    This phase only reads the world → it parallelizes for a large dirty-set.
    {
        let TransformScratch {
            dirty_entities,
            dirty,
            stack,
            ..
        } = &mut scratch;
        let dirty: &IndexStamp = dirty;
        let seed = |world: &World, entity: Entity| -> Option<(Entity, GlobalTransform)> {
            let parent = world.target_of(entity, ChildOf);
            let mut ancestor = parent;
            let mut depth = 0usize;
            while let Some(p) = ancestor {
                if dirty.contains(p.index) {
                    return None; // covered by a dirty ancestor — recomputed by its descent
                }
                ancestor = world.target_of(p, ChildOf);
                depth += 1;
                if depth > MAX_PROPAGATE_DEPTH {
                    // Defensive: a ChildOf cycle would loop here forever.
                    // `add_relation` now rejects cycle-forming edges, so this is
                    // unreachable in practice — treat the chain as a clean root
                    // rather than hanging the frame.
                    log::error!(
                        "propagate_transforms: ChildOf ancestor chain for {entity} exceeds depth limit — possible cycle"
                    );
                    break;
                }
            }
            // The parent is clean (or absent) — its world matrix is valid from previous
            // frames; a missing GlobalTransform is treated as identity (previous semantics).
            let parent_global = parent
                .and_then(|p| world.get::<GlobalTransform>(p))
                .copied()
                .unwrap_or(GlobalTransform::IDENTITY);
            Some((entity, parent_global))
        };
        const PAR_MIN_DIRTY: usize = 4096;
        if dirty_entities.len() >= PAR_MIN_DIRTY {
            use rayon::prelude::*;
            let world_ref: &World = world;
            stack.par_extend(
                dirty_entities
                    .par_iter()
                    .filter_map(|&e| seed(world_ref, e)),
            );
        } else {
            for &entity in dirty_entities.iter() {
                if let Some(s) = seed(world, entity) {
                    stack.push(s);
                }
            }
        }
    }
    let seeds = scratch.stack.len();
    let t2 = std::time::Instant::now();

    // 3. Descent. Two modes with identical semantics:
    //    — few roots / small subtrees → sequential in-place DFS;
    //    — many independent roots (typical: a crowd of animated characters) →
    //      phase A: parallel matrix computation over disjoint subtrees
    //      (read-only &World — Sync, as in parallel queries), phase B:
    //      sequential writes (get_mut/insert require &mut World).
    //      The subtrees are disjoint by construction (an entity has one parent, roots are not
    //      nested within each other), so the writes do not conflict and the order of phase B
    //      does not matter.
    //    Strategy: **widen-then-descend**. Parallelism over ROOTS is efficient (thread-local stack,
    //    no materialization of levels), but degrades when there are few roots and the subtrees are
    //    huge (ring → 10k foxes → 260k nodes: 56 roots). So FIRST we widen the frontier of roots with
    //    cheap sequential levels until it becomes wide (56 → 10000 foxes — we process only
    //    56 nodes), THEN — a parallel descent over the independent subtrees of the wide frontier. If
    //    the frontier is already wide (10000 animated character-roots) — the widening is skipped; if
    //    the tree is narrow and does not widen (a chain) — we bail out and descend as is.
    let mut visits = 0usize;
    let gt_id = world.registry.get_id::<GlobalTransform>();
    // Auto-creation of GlobalTransform (entity with a single LocalTransform): empty on the hot path
    // (required components); the structural insert happens at the end, not during the parallel descent.
    let mut missing: Vec<(Entity, GlobalTransform)> = Vec::new();
    let mut frontier: Vec<(Entity, GlobalTransform)> = scratch.stack.drain(..).collect();

    // ── Widen phase: sequentially process the top (narrow) levels until the frontier becomes
    //    wide enough for good parallelism over subtrees. ──
    const WIDE_ENOUGH: usize = 1024;
    loop {
        if frontier.len() >= WIDE_ENOUGH || frontier.is_empty() {
            break;
        }
        let prev_len = frontier.len();
        let mut next: Vec<(Entity, GlobalTransform)> = Vec::new();
        for &(entity, pg) in &frontier {
            if !world.is_alive(entity) {
                continue;
            }
            // An entity without a LocalTransform stops the descent (the cascade goes only through nodes with a transform).
            let local = match world.get::<LocalTransform>(entity) {
                Some(l) => *l,
                None => continue,
            };
            let global = pg.mul_local(&local);
            visits += 1;
            if let Some(mut gt) = world.get_mut::<GlobalTransform>(entity) {
                *gt = global;
            } else {
                missing.push((entity, global));
            }
            for child in world.targets_of(ChildOf, entity) {
                next.push((child, global));
            }
        }
        let grew = next.len() > prev_len;
        frontier = next;
        if !grew {
            // The level does not widen (a narrow/chained tree) — no point in widening further.
            break;
        }
    }

    // ── Descend phase: parallel descent over the independent subtrees of the wide frontier. ──
    const PAR_MIN_ROOTS: usize = 64;
    if frontier.len() >= PAR_MIN_ROOTS {
        use rayon::prelude::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        // Writes go DIRECTLY from the parallel descent via (archetype, row) pointers — the same
        // contract as parallel Write-queries (a row is written by exactly one thread): subtrees are
        // disjoint, an entity is visited once; there are no structural changes in this phase (missing is deferred).
        let roots = frontier;
        let missing_par: std::sync::Mutex<Vec<(Entity, GlobalTransform)>> =
            std::sync::Mutex::new(Vec::new());
        let visited = AtomicUsize::new(0);
        let world_ref: &World = world;
        roots.par_iter().for_each(|&(root, parent_global)| {
            let mut stack: Vec<(Entity, GlobalTransform)> = Vec::with_capacity(64);
            stack.push((root, parent_global));
            let mut local_missing: Vec<(Entity, GlobalTransform)> = Vec::new();
            let mut n = 0usize;
            while let Some((entity, pg)) = stack.pop() {
                if !world_ref.is_alive(entity) {
                    continue;
                }
                let local = match world_ref.get::<LocalTransform>(entity) {
                    Some(l) => *l,
                    None => continue,
                };
                let global = pg.mul_local(&local);
                n += 1;
                if !write_global_parallel(world_ref, gt_id, entity, global, this_run) {
                    local_missing.push((entity, global));
                }
                for child in world_ref.targets_of(ChildOf, entity) {
                    stack.push((child, global));
                }
            }
            if n > 0 {
                visited.fetch_add(n, Ordering::Relaxed);
            }
            if !local_missing.is_empty() {
                missing_par.lock().unwrap().extend(local_missing);
            }
        });
        visits += visited.load(Ordering::Relaxed);
        missing.extend(missing_par.into_inner().unwrap());
    } else {
        // Sequential DFS of the remainder (narrow frontier): each node exactly once, after its parent.
        let mut stack = frontier;
        while let Some((entity, parent_global)) = stack.pop() {
            if !world.is_alive(entity) {
                continue;
            }
            let local = match world.get::<LocalTransform>(entity) {
                Some(l) => *l,
                None => continue,
            };
            let global = parent_global.mul_local(&local);
            visits += 1;
            if let Some(mut gt) = world.get_mut::<GlobalTransform>(entity) {
                *gt = global;
            } else {
                missing.push((entity, global));
            }
            for child in world.targets_of(ChildOf, entity) {
                stack.push((child, global));
            }
        }
    }

    // Auto-initialize missing GlobalTransforms (each entity is visited exactly once).
    for (entity, global) in missing {
        world.insert(entity, global);
    }

    if trace {
        log::info!(
            "PROP_TRACE: total {:.2}ms | changed-query {:.2} | seed-roots {:.2} | descend {:.2} \
             | dirty {} roots {} visits {}",
            t0.elapsed().as_secs_f64() * 1000.0,
            (t1 - t0).as_secs_f64() * 1000.0,
            (t2 - t1).as_secs_f64() * 1000.0,
            t2.elapsed().as_secs_f64() * 1000.0,
            scratch.dirty_entities.len(),
            seeds,
            visits
        );
    }

    // Commit this run's tick and return the scratch for reuse.
    scratch.last_run = this_run;
    world.insert_resource(scratch);
}

/// Direct write of `GlobalTransform` via (archetype, row) from the parallel descent.
/// Returns `false` if the entity does NOT have the component yet (a deferred insert is needed);
/// dead/invalid rows are silently ignored (`true`) — like an is_alive filter.
///
/// Safety contract (the same as parallel Write-queries): the caller
/// guarantees that (a) each entity is written by at most one thread and
/// (b) there are no structural changes to the world during the phase (spawn/despawn/insert/remove).
fn write_global_parallel(
    world: &World,
    gt_id: Option<crate::component::ComponentId>,
    entity: Entity,
    global: GlobalTransform,
    tick: Tick,
) -> bool {
    let Some(cid) = gt_id else {
        // The component is not registered yet (the very first frame) → deferred insert.
        return false;
    };
    let Some(loc) = world.entities.get_location(entity) else {
        return true;
    };
    let arch = &world.archetypes[loc.archetype_id.as_usize()];
    let Some(col_idx) = arch.column_index(cid) else {
        return false;
    };
    let col = &arch.columns[col_idx];
    let row = loc.row as usize;
    if row >= col.len {
        return true;
    }
    // SAFETY: the row is valid (row < len); exclusive access to the row and the absence of structural
    // changes are the caller's contract (see doc). GlobalTransform: Copy, no Drop — the pointer
    // cast is layout-agnostic (the column was created for THIS GlobalTransform type; the C2
    // Mat4 → { Mat3, DVec3 } re-shape keeps size 64 B / raises align to 8, both properties of
    // the registered component, so the column stride/align already match).
    unsafe {
        *(col.get_ptr(row) as *mut GlobalTransform) = global;
        col.set_change_tick(row, tick);
    }
    true
}

// ── Plugin ───────────────────────────────────────────────────────

/// Plugin for registering Transform components.
///
/// Registers [`LocalTransform`], [`GlobalTransform`] and [`TransformDirty`].
///
/// # Adding the system
///
/// The `propagate_transforms` system is added to the Scheduler manually:
///
/// ```ignore
/// use apex_scheduler::stage::StageLabel;
///
/// scheduler.add_system_to_stage(
///     "propagate_transforms",
///     apex_core::transform::propagate_transforms,
///     StageLabel::PostUpdate,
/// );
/// ```
pub struct TransformPlugin;

impl TransformPlugin {
    /// (Optionally) pre-initialize the Transform state in the World.
    ///
    /// **Component registration is no longer needed** — `LocalTransform`/`GlobalTransform`
    /// are marked `#[derive(Component)]` and auto-register on `World::new()`
    /// (linkme). `TransformDirty` and the write-hook are removed (dirty-detection — via
    /// `Changed<LocalTransform>`, C1). This function only pre-creates the `propagate_transforms`
    /// scratch buffer (which is also created lazily on the first run).
    pub fn register_components(world: &mut World) {
        world.insert_resource(TransformScratch::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{Changed, Query};
    use crate::world::World;

    #[test]
    fn local_transform_default_is_identity() {
        let lt = LocalTransform::default();
        assert_eq!(lt.translation, DVec3::ZERO);
        assert_eq!(lt.rotation, Quat::IDENTITY);
        assert_eq!(lt.scale, Vec3::ONE);
    }

    #[test]
    fn looking_at_points_forward_at_target_without_roll() {
        let eye = DVec3::new(3.0, 4.0, 5.0);
        let target = DVec3::new(0.0, 1.0, 0.0);
        let t = LocalTransform::from_translation(eye).looking_at(target, Vec3::Y);

        // forward (−Z) points exactly at target
        let expected = (target - eye).as_vec3().normalize();
        assert!((t.forward() - expected).length() < 1e-6);
        // no roll: right is horizontal (⊥ to world Y)
        assert!(t.right().dot(Vec3::Y).abs() < 1e-6);
        // orthonormality of the basis
        assert!((t.up().dot(t.forward())).abs() < 1e-6);
        assert!((t.rotation.length() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn looking_at_degenerate_inputs_do_not_produce_nan() {
        // direction == 0 (target == eye) and up ∥ direction — must not produce NaN.
        let t =
            LocalTransform::from_xyz(1.0, 2.0, 3.0).looking_at(DVec3::new(1.0, 2.0, 3.0), Vec3::Y);
        assert!(t.rotation.is_finite());
        let t = LocalTransform::IDENTITY.looking_to(Vec3::Y, Vec3::Y);
        assert!(t.rotation.is_finite());
        assert!((t.forward() - Vec3::Y).length() < 1e-6);
    }

    #[test]
    fn global_transform_constructors_match_local() {
        let eye = DVec3::new(0.0, 10.0, 5.0);
        let target = DVec3::ZERO;
        let local = LocalTransform::from_translation(eye).looking_at(target, Vec3::Y);
        let global = GlobalTransform::looking_at(eye, target, Vec3::Y);
        let from_local: GlobalTransform = (&local).into();
        // The transforms match (scale=1 in both paths).
        assert!((global.translation - from_local.translation).length() < 1e-9);
        let (da, db) = (global.forward(), from_local.forward());
        assert!((da - db).length() < 1e-6);
        let gt = GlobalTransform::from_xyz(1.0, 2.0, 3.0);
        assert_eq!(gt.translation, DVec3::new(1.0, 2.0, 3.0));
        assert_eq!(gt.m3, Mat3::IDENTITY);
    }

    #[test]
    fn direction_accessors_match_rotation() {
        let t = LocalTransform::from_rotation(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2));
        // Rotation of +90° around Y: forward (−Z) → −X.
        assert!((t.forward() - Vec3::NEG_X).length() < 1e-6);
        assert!((t.back() + t.forward()).length() < 1e-6);
        assert!((t.left() + t.right()).length() < 1e-6);
        assert!((t.down() + t.up()).length() < 1e-6);
    }

    #[test]
    fn local_transform_to_matrix() {
        let lt = LocalTransform::from_translation(DVec3::new(1.0, 2.0, 3.0));
        let m = lt.to_matrix();
        // Verify that the 4x4 matrix maps the origin to the translation
        let origin = Vec3::ZERO;
        let transformed = m.transform_point3(origin);
        assert_eq!(transformed, Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn global_transform_default_is_identity() {
        let gt = GlobalTransform::default();
        assert_eq!(gt.to_mat4_lossy(), Mat4::IDENTITY);
        assert_eq!(gt.rebased_mat4(DVec3::ZERO), Mat4::IDENTITY);
    }

    /// C6: `LocalTransform`/`GlobalTransform` auto-register on `World::new()`
    /// via `#[derive(Component)]` (linkme) — without a manual `register_component`.
    ///
    /// Under Miri (and wasm32) `linkme::distributed_slice` is disabled — components
    /// register lazily on spawn/insert (see `component.rs`, TD-25), so
    /// auto-registration on an empty `World::new()` does not happen. This test checks
    /// exactly the linkme path → we skip it in these configurations.
    #[cfg_attr(any(miri, target_arch = "wasm32"), ignore)]
    #[test]
    fn transform_components_auto_registered() {
        let world = World::new();
        assert!(
            world.registry().get_id::<LocalTransform>().is_some(),
            "LocalTransform must auto-register via derive(Component)"
        );
        assert!(
            world.registry().get_id::<GlobalTransform>().is_some(),
            "GlobalTransform must auto-register via derive(Component)"
        );
    }

    #[test]
    fn propagate_single_entity_auto_init_global() {
        // WITHOUT register_components: derive auto-registers the components,
        // scratch is created lazily in propagate.
        let mut world = World::new();

        // Spawn with a SINGLE LocalTransform — no GlobalTransform, no TransformDirty.
        let entity = world.spawn((LocalTransform::from_translation(DVec3::new(10.0, 0.0, 0.0)),));

        // GlobalTransform does not exist yet.
        assert!(world.get::<GlobalTransform>(entity).is_none());

        propagate_transforms(&mut world);

        // propagate auto-initialized GlobalTransform = LocalTransform.
        let gt = world.get::<GlobalTransform>(entity).unwrap();
        assert_eq!(gt.translation, DVec3::new(10.0, 0.0, 0.0));
    }

    #[test]
    fn propagate_parent_child_chain() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        // Hierarchy parent → child, both with a single LocalTransform (GlobalTransform auto).
        let parent =
            world.spawn((LocalTransform::from_translation(DVec3::new(100.0, 0.0, 0.0)),));
        let child = world.spawn((LocalTransform::from_translation(DVec3::new(10.0, 0.0, 0.0)),));
        world.add_relation(child, ChildOf, parent);

        propagate_transforms(&mut world);

        // child.Global = parent.Global * child.Local = (100 + 10) = 110 on X.
        let child_gt = world.get::<GlobalTransform>(child).unwrap();
        assert_eq!(
            child_gt.translation,
            DVec3::new(110.0, 0.0, 0.0),
            "Child must be at 110.0 on X (100 parent + 10 local)"
        );
        let parent_gt = world.get::<GlobalTransform>(parent).unwrap();
        assert_eq!(parent_gt.translation, DVec3::new(100.0, 0.0, 0.0));
    }

    #[test]
    fn propagate_deep_hierarchy() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        // grandparent → parent → child
        let grandparent = world.spawn((
            LocalTransform::from_translation(DVec3::new(50.0, 0.0, 0.0)),
            GlobalTransform::default(),
        ));

        let parent = world.spawn((LocalTransform::from_translation(DVec3::new(30.0, 0.0, 0.0)),));
        let child = world.spawn((LocalTransform::from_translation(DVec3::new(20.0, 0.0, 0.0)),));

        world.add_relation(parent, ChildOf, grandparent);
        world.add_relation(child, ChildOf, parent);

        propagate_transforms(&mut world);

        // parent = 50 + 30 = 80
        let parent_gt = world.get::<GlobalTransform>(parent).unwrap();
        assert_eq!(
            parent_gt.translation,
            DVec3::new(80.0, 0.0, 0.0),
            "Parent must be at 80.0"
        );

        // child = 80 + 20 = 100
        let child_gt = world.get::<GlobalTransform>(child).unwrap();
        assert_eq!(
            child_gt.translation,
            DVec3::new(100.0, 0.0, 0.0),
            "Child must be at 100.0"
        );
    }

    /// Key C1+C2: mutating `LocalTransform` via `Query<&mut _>` (without
    /// a manual `TransformDirty`) triggers a recompute of `GlobalTransform`.
    #[test]
    fn changed_local_via_write_query_triggers_recompute() {

        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let e = world.spawn((LocalTransform::from_translation(DVec3::new(1.0, 0.0, 0.0)),));

        // First pass: auto-init GlobalTransform = (1,0,0).
        propagate_transforms(&mut world);
        assert_eq!(
            world.get::<GlobalTransform>(e).unwrap().translation,
            DVec3::new(1.0, 0.0, 0.0)
        );

        // Advance the tick (as a frame does) and mutate via Query<Write> — without
        // any manual marker.
        world.tick();
        {
            let mut q = Query::<&mut LocalTransform>::new_mut(&mut world);
            q.for_each_mut(|_, mut lt| {
                lt.translation = DVec3::new(42.0, 0.0, 0.0);
            });
        }

        propagate_transforms(&mut world);

        // GlobalTransform recomputed without TransformDirty.
        assert_eq!(
            world.get::<GlobalTransform>(e).unwrap().translation,
            DVec3::new(42.0, 0.0, 0.0),
            "Changed<LocalTransform> via Query<Write> must trigger a recompute"
        );
    }

    /// A dirty node under a CLEAN intermediate parent with a dirty grandparent: the descent
    /// from the dirty-root must recompute it exactly once and STRICTLY after
    /// the grandparent (the former toposort ordered it before the ancestor's recompute and
    /// fixed the result with a second cascade pass).
    #[test]
    fn dirty_leaf_under_clean_intermediate_uses_fresh_ancestor_global() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let gp = world.spawn((LocalTransform::from_translation(DVec3::new(1.0, 0.0, 0.0)),));
        let mid = world.spawn((LocalTransform::from_translation(DVec3::new(10.0, 0.0, 0.0)),));
        let leaf = world.spawn((LocalTransform::from_translation(DVec3::new(100.0, 0.0, 0.0)),));
        world.add_relation(mid, ChildOf, gp);
        world.add_relation(leaf, ChildOf, mid);

        propagate_transforms(&mut world);
        assert_eq!(
            world.get::<GlobalTransform>(leaf).unwrap().translation,
            DVec3::new(111.0, 0.0, 0.0)
        );

        // Change the grandparent and the leaf; the intermediate node stays clean.
        world.tick();
        world.get_mut::<LocalTransform>(gp).unwrap().translation = DVec3::new(2.0, 0.0, 0.0);
        world.get_mut::<LocalTransform>(leaf).unwrap().translation = DVec3::new(200.0, 0.0, 0.0);
        propagate_transforms(&mut world);

        // 2 (gp) + 10 (mid, clean) + 200 (leaf) = 212.
        assert_eq!(
            world.get::<GlobalTransform>(leaf).unwrap().translation,
            DVec3::new(212.0, 0.0, 0.0),
            "the leaf must be computed from the FRESH grandparent matrix through the clean intermediate node"
        );
        // The clean intermediate node is also recomputed (it is in the dirty-root's subtree).
        assert_eq!(
            world.get::<GlobalTransform>(mid).unwrap().translation,
            DVec3::new(12.0, 0.0, 0.0)
        );
    }

    /// The direct scan of tick columns (phase 1) must yield exactly the same dirty-set as
    /// the reference `Query<Changed<LocalTransform>>` from the same base.
    #[test]
    fn direct_tick_scan_matches_changed_query() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);
        let mut all = Vec::new();
        for i in 0..100 {
            all.push(world.spawn((LocalTransform::from_translation(DVec3::new(
                i as f64, 0.0, 0.0,
            )),)));
        }
        // Half of the entities — with a different composition (a different archetype in the scan).
        for (i, &e) in all.iter().enumerate() {
            if i % 2 == 0 {
                world.insert(e, GlobalTransform::IDENTITY);
            }
        }
        propagate_transforms(&mut world);
        let last_run = world.resource::<TransformScratch>().last_run;

        world.tick();
        for (i, &e) in all.iter().enumerate() {
            if i % 3 == 0 {
                world.get_mut::<LocalTransform>(e).unwrap().translation.y = 5.0;
            }
        }

        // Reference — a generic query from the same base.
        let mut expected: Vec<Entity> = Vec::new();
        {
            let q = Query::<Changed<LocalTransform>>::new_with_tick(&world, last_run);
            q.for_each(|e, _| expected.push(e));
        }

        propagate_transforms(&mut world);
        let mut actual = world.resource::<TransformScratch>().dirty_entities.clone();
        expected.sort_by_key(|e| e.index);
        actual.sort_by_key(|e| e.index);
        assert!(!expected.is_empty());
        assert_eq!(actual, expected, "the direct tick scan must match the Changed query");
    }

    /// The parallel descent branch (≥64 independent dirty-roots): the result is identical
    /// to the sequential one — each child's global = parent.local * child.local.
    #[test]
    fn many_dirty_roots_take_parallel_descent_and_stay_correct() {

        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let n = 200; // > PAR_MIN_ROOTS
        let mut pairs = Vec::new();
        for i in 0..n {
            let parent =
                world.spawn((LocalTransform::from_translation(DVec3::new(i as f64, 0.0, 0.0)),));
            let child =
                world.spawn((LocalTransform::from_translation(DVec3::new(0.0, 1.0, 0.0)),));
            world.add_relation(child, ChildOf, parent);
            pairs.push((parent, child));
        }
        propagate_transforms(&mut world);

        // All parents are dirty at once → n independent subtrees → the parallel branch.
        world.tick();
        {
            let mut q = Query::<&mut LocalTransform>::new_mut(&mut world);
            q.for_each_mut(|_, mut lt| {
                if lt.translation.y == 0.0 {
                    lt.translation.x += 1000.0;
                }
            });
        }
        propagate_transforms(&mut world);

        for (i, (parent, child)) in pairs.iter().enumerate() {
            assert_eq!(
                world.get::<GlobalTransform>(*parent).unwrap().translation,
                DVec3::new(1000.0 + i as f64, 0.0, 0.0)
            );
            assert_eq!(
                world.get::<GlobalTransform>(*child).unwrap().translation,
                DVec3::new(1000.0 + i as f64, 1.0, 0.0),
                "child {i} must be recomputed from the fresh parent in the parallel branch"
            );
        }
    }

    /// A deep/wide hierarchy with FEW roots (the ring-parent many_foxes case): parallelism
    /// must go by the WIDTH of a level, not by the number of roots. 1 root → 300 children (a wide level >
    /// PAR_MIN_LEVEL → the parallel branch) → each with a grandchild. Moving ONLY the root cascades to 600
    /// descendants; the level's parent is passed to children by value across the level barrier.
    #[test]
    fn deep_wide_hierarchy_few_roots_propagates_in_parallel_levels() {

        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let root = world.spawn((LocalTransform::from_translation(DVec3::new(10.0, 0.0, 0.0)),));
        let mut grandkids = Vec::new();
        for i in 0..300 {
            let child =
                world.spawn((LocalTransform::from_translation(DVec3::new(0.0, i as f64, 0.0)),));
            world.add_relation(child, ChildOf, root);
            let gk = world.spawn((LocalTransform::from_translation(DVec3::new(0.0, 0.0, 1.0)),));
            world.add_relation(gk, ChildOf, child);
            grandkids.push((i, gk));
        }
        propagate_transforms(&mut world);

        // Move ONLY the root → the recompute cascades to 600 descendants through wide levels.
        world.tick();
        {
            let mut q = Query::<&mut LocalTransform>::new_mut(&mut world);
            q.for_each_mut(|e, mut lt| {
                if e == root {
                    lt.translation.x = 1000.0;
                }
            });
        }
        propagate_transforms(&mut world);

        // Grandchild i: root(1000,0,0) + child(0,i,0) + gk(0,0,1) = (1000, i, 1).
        for (i, gk) in grandkids {
            assert_eq!(
                world.get::<GlobalTransform>(gk).unwrap().translation,
                DVec3::new(1000.0, i as f64, 1.0),
                "grandchild {i} recomputed from the fresh root through the parallel level"
            );
        }
    }

    /// Changing only the parent cascades the recompute to the children.
    #[test]
    fn parent_change_cascades_to_children() {

        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let parent = world.spawn((LocalTransform::from_translation(DVec3::new(0.0, 0.0, 0.0)),));
        let child = world.spawn((LocalTransform::from_translation(DVec3::new(5.0, 0.0, 0.0)),));
        world.add_relation(child, ChildOf, parent);

        propagate_transforms(&mut world);
        assert_eq!(
            world.get::<GlobalTransform>(child).unwrap().translation,
            DVec3::new(5.0, 0.0, 0.0)
        );

        // Move ONLY the parent.
        world.tick();
        {
            let mut q = Query::<&mut LocalTransform>::new_mut(&mut world);
            q.for_each_mut(|e, mut lt| {
                if e == parent {
                    lt.translation = DVec3::new(100.0, 0.0, 0.0);
                }
            });
        }
        propagate_transforms(&mut world);

        // The child is recomputed by cascade: 100 (parent) + 5 (local) = 105.
        assert_eq!(
            world.get::<GlobalTransform>(child).unwrap().translation,
            DVec3::new(105.0, 0.0, 0.0),
            "changing the parent must cascade a recompute to the child"
        );
    }

    #[test]
    fn transform_directions_match_bevy_signs() {
        // 1:1 Bevy: forward = −Z, back = +Z, right = +X, left = −X, up = +Y, down = −Y. Pins the
        // sign convention so the spot-shadow basis flip (TECH_DEBT 2026-06-15) can't recur silently.
        // Identity transform → world axes line up with local.
        let m = Mat4::IDENTITY;
        assert!((m.forward() - Vec3::NEG_Z).length() < 1e-6);
        assert!((m.back() - Vec3::Z).length() < 1e-6);
        assert!((m.right() - Vec3::X).length() < 1e-6);
        assert!((m.left() - Vec3::NEG_X).length() < 1e-6);
        assert!((m.up() - Vec3::Y).length() < 1e-6);
        assert!((m.down() - Vec3::NEG_Y).length() < 1e-6);

        // forward()/back() are exact opposites; `GlobalTransform` mirrors the same convention and
        // agrees with `LocalTransform` (built from the same rotation) for an arbitrary orientation.
        let lt =
            LocalTransform::from_xyz(1.0, 2.0, 3.0).looking_at(DVec3::new(4.0, 0.0, -1.0), Vec3::Y);
        let gt = GlobalTransform::from_mat4(lt.to_matrix());
        assert!((gt.forward() - lt.forward()).length() < 1e-5, "GlobalTransform.forward == LocalTransform.forward");
        assert!((gt.back() - lt.back()).length() < 1e-5);
        assert!((gt.right() - lt.right()).length() < 1e-5);
        assert!((gt.up() - lt.up()).length() < 1e-5);
        assert!((gt.forward() + gt.back()).length() < 1e-5, "forward = -back");
    }

    /// The C2 authoring gate at the CORE level (engine ADR-056 / TD-203): a
    /// 1 cm authored step a thousand km out survives the write, the
    /// propagation (incl. a deep chain) and the window narrowing. In f32 the
    /// step would be swallowed whole (ulp(10⁶) = 6.25 cm).
    #[test]
    fn f64_translation_survives_centimetre_steps_a_thousand_km_out() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let parent =
            world.spawn((LocalTransform::from_translation(DVec3::new(1.0e6, 0.0, 0.0)),));
        let child = world.spawn((LocalTransform::from_translation(DVec3::new(2.0, 0.0, 0.0)),));
        world.add_relation(child, ChildOf, parent);
        propagate_transforms(&mut world);

        // The authored step: += 1 cm on the parent, a thousand km out.
        world.tick();
        world.get_mut::<LocalTransform>(parent).unwrap().translation.x += 0.01;
        propagate_transforms(&mut world);

        let pt = world.get::<GlobalTransform>(parent).unwrap().translation;
        let ct = world.get::<GlobalTransform>(child).unwrap().translation;
        assert_eq!(pt.x, 1.0e6 + 0.01, "the centimetre write must be stored exactly (f64)");
        assert_eq!(ct.x, 1.0e6 + 0.01 + 2.0, "…and propagate exactly through the chain");

        // The window narrowing keeps it: rebased against an anchor near the
        // camera, the centimetre is far above the local f32 ulp.
        let origin = DVec3::new(1.0e6, 0.0, 0.0);
        let before = world.get::<GlobalTransform>(parent).unwrap().rebased_mat4(origin);
        assert!((before.w_axis.x - 0.01).abs() < 1e-6, "1 cm survives the rebased narrowing");
    }

    /// The hybrid `{ Mat3, DVec3 }` composition must match the old Mat4×Mat4
    /// reference bit-for-bit in the linear part (that is what keeps the
    /// engine's goldens byte-identical) and to f32-ulp in the translation.
    #[test]
    fn hybrid_composition_matches_mat4_reference() {
        let parent = LocalTransform {
            translation: DVec3::new(3.5, -2.25, 7.125),
            rotation: Quat::from_euler(glam::EulerRot::YXZ, 0.7, -0.3, 0.15),
            scale: Vec3::new(2.0, 1.0, 0.5), // non-uniform — shear appears downstream
        };
        let child = LocalTransform {
            translation: DVec3::new(-1.5, 4.0, 2.5),
            rotation: Quat::from_euler(glam::EulerRot::YXZ, -0.2, 0.9, 0.4),
            scale: Vec3::new(1.5, 3.0, 1.0),
        };
        let hybrid = GlobalTransform::from(&parent).mul_local(&child);
        let reference = parent.to_matrix() * child.to_matrix();
        assert_eq!(
            hybrid.m3,
            Mat3::from_mat4(reference),
            "the linear part must be bit-identical to the Mat4 reference composition \
             (incl. shear from the non-uniform parent scale)"
        );
        let rt = reference.w_axis.truncate();
        let ht = hybrid.translation.as_vec3();
        assert!(
            (rt - ht).length() < 1e-5,
            "the f64 translation must match the f32 reference within f32 rounding"
        );
    }
}
