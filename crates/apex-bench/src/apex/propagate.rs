use apex_core::prelude::*;
use apex_core::transform::{propagate_transforms, GlobalTransform, LocalTransform, TransformPlugin};
use glam::{DVec3, Quat, Vec3};

// Propagate — hierarchical transform propagation (our differentiator; perf-sensitive, in the past
// there was a perf bug of subtree duplication). 200 roots × a chain of 50 = 10k nodes; every frame all
// LocalTransforms move (worst-case crowd, the whole graph dirty) + propagation. Apex focus (bevy
// propagate — a separate crate/schedule); a criterion guard against propagation regressions.
fn lt(seed: f32) -> LocalTransform {
    LocalTransform {
        translation: DVec3::new(seed as f64, 0.0, 0.0),
        rotation: Quat::from_rotation_y(0.01 * seed),
        scale: Vec3::ONE,
    }
}

pub struct Propagate {
    world: World,
    nodes: Vec<Entity>,
}

impl Default for Propagate {
    fn default() -> Self {
        Self::new()
    }
}

impl Propagate {
    pub fn new() -> Self {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);
        let mut nodes = Vec::with_capacity(10_000);
        for r in 0..200 {
            let root = world.spawn((lt(r as f32), GlobalTransform::IDENTITY));
            nodes.push(root);
            let mut parent = root;
            for d in 0..49 {
                let child = world.spawn((lt(d as f32), GlobalTransform::IDENTITY));
                world.add_relation(child, ChildOf, parent);
                nodes.push(child);
                parent = child;
            }
        }
        propagate_transforms(&mut world); // initial propagation (stabilizes the cache)
        Self { world, nodes }
    }

    pub fn run(&mut self) {
        // Crowd frame: move all LocalTransforms (stamps dirty) + propagation over the whole graph.
        self.world.tick();
        for &e in &self.nodes {
            if let Some(mut l) = self.world.get_mut::<LocalTransform>(e) {
                l.translation.x += 0.001;
            }
        }
        propagate_transforms(&mut self.world);
    }
}

// PropagateStatic — the PE-C1 target case: the same 10k-node hierarchy, but NOTHING moves.
// Measures the pure per-frame cost of propagate proving there is no work: pre PE-C1/C2 a
// linear tick scan of every LocalTransform row, post — an O(archetypes) aggregate check.
pub struct PropagateStatic {
    world: World,
}

impl Default for PropagateStatic {
    fn default() -> Self {
        Self::new()
    }
}

impl PropagateStatic {
    pub fn new() -> Self {
        let mut inner = Propagate::new();
        // Settle: one propagated frame, then the graph goes quiet.
        inner.run();
        Self { world: inner.world }
    }

    pub fn run(&mut self) {
        self.world.tick();
        propagate_transforms(&mut self.world);
    }
}
