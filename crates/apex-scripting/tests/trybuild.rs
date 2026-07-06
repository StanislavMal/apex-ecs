//! Compile-fail tests for `#[derive(Scriptable)]`.
//!
//! The derive rejects shapes it cannot map onto a Lua table (an enum with data,
//! a union, a field-less tuple struct) with its OWN diagnostic that names the
//! manual-impl escape hatch. Each `.stderr` snapshot pins that message.

#[test]
fn compile_fail() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
