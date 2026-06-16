use apex_core::prelude::*;
use apex_core::transform::{propagate_transforms, GlobalTransform, LocalTransform, TransformPlugin};
use glam::{Mat4, Quat, Vec3};

// Propagate — иерархическая пропагация трансформов (наш дифференциатор; перф-чувствительна, в прошлом
// был перф-баг дублирования поддеревьев). 200 корней × цепочка из 50 = 10k узлов; каждый кадр все
// LocalTransform двигаются (worst-case толпа, весь граф dirty) + пропагация. Apex-фокус (bevy
// propagate — отдельный crate/schedule); criterion-страж от регрессий пропагации.
fn lt(seed: f32) -> LocalTransform {
    LocalTransform {
        translation: Vec3::new(seed, 0.0, 0.0),
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
            let root = world.spawn((lt(r as f32), GlobalTransform(Mat4::IDENTITY)));
            nodes.push(root);
            let mut parent = root;
            for d in 0..49 {
                let child = world.spawn((lt(d as f32), GlobalTransform(Mat4::IDENTITY)));
                world.add_relation(child, ChildOf, parent);
                nodes.push(child);
                parent = child;
            }
        }
        propagate_transforms(&mut world); // первичная пропагация (стабилизирует кэш)
        Self { world, nodes }
    }

    pub fn run(&mut self) {
        // Кадр толпы: двигаем все LocalTransform (стампит dirty) + пропагация по всему графу.
        self.world.tick();
        for &e in &self.nodes {
            if let Some(mut l) = self.world.get_mut::<LocalTransform>(e) {
                l.translation.x += 0.001;
            }
        }
        propagate_transforms(&mut self.world);
    }
}
