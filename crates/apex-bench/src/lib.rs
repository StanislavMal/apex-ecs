//! Apex ECS benchmark suite
//!
//! Provides a common [`Benchmark`] trait and cgmath-based components
//! used across all benchmark implementations (apex, bevy, legion).

use cgmath::{Matrix4, Vector3};

// ---------------------------------------------------------------------------
// Common benchmark components
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Transform(pub Matrix4<f32>);

#[derive(Clone, Copy)]
pub struct Position(pub Vector3<f32>);

#[derive(Clone, Copy)]
pub struct Rotation(pub Vector3<f32>);

#[derive(Clone, Copy)]
pub struct Velocity(pub Vector3<f32>);

// ---------------------------------------------------------------------------
// Benchmark trait – every benchmark must implement this
// ---------------------------------------------------------------------------

pub trait Benchmark {
    fn new() -> Self;
    fn run(&mut self);
}

// ---------------------------------------------------------------------------
// Benchmark modules (всегда доступны, feature-gating внутри)
// ---------------------------------------------------------------------------
pub mod apex;
#[cfg(feature = "bevy")]
pub mod bevy;
#[cfg(feature = "legion")]
pub mod legion;
