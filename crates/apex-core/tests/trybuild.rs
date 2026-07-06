//! Compile-fail tests for the `#[derive(Bundle)]` guard and the `system!`
//! migrant traps.
//!
//! Each fixture asserts that a misuse fails with OUR OWN, specific diagnostic
//! (a `compile_error!` spelling out the fix), not with a downstream generic
//! type error. The `.stderr` snapshot next to each fixture pins the exact
//! message the user hitting the trap will read — that message IS the artifact
//! under test (§0.2a: fail loudly, name the fix).

#[test]
fn compile_fail() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
