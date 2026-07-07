//! Authored visibility — the scene-graph show/hide flag.
//!
//! [`Visibility`] is the **authored intent** (like `LocalTransform`): an optional component where
//! `Inherited` (default) defers to the `ChildOf` parent and `Visible`/`Hidden` force this entity and its
//! `Inherited` descendants; an absent component means visible.
//!
//! It lives in core (not the renderer) so authored tools can set and serialize it without depending on
//! the render crate — the editor is render-agnostic, yet the authored hide flag must be the SAME type
//! the renderer honors (no per-crate mirror). The DERIVED render-side effective visibility
//! (`InheritedVisibility`), its propagation, and the extract integration stay in the renderer, which
//! computes them from this authored chain.
//!
//! Serde is derived but never forces serialization: a `World` snapshot only writes components whose
//! serde is REGISTERED. Game/engine worlds do not register `Visibility` (it is re-derived on load); the
//! editor's document world opts in (per-world), so authored hides persist there without changing the
//! engine's byte-identical default.

use crate::component::Component;

/// Authored visibility of an entity. Optional — an absent component means visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Visibility {
    /// Inherit the parent's effective visibility (root default: visible).
    #[default]
    Inherited,
    /// Force visible, regardless of an inherited-hidden ancestor.
    Visible,
    /// Hide this entity and its `Inherited` descendants.
    Hidden,
}

impl Component for Visibility {}
