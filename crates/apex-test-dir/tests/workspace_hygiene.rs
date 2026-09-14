//! The core's half of the scratch-directory rule: no test of this workspace spells a temporary
//! directory by hand (the shapes and their reasons: `apex_test_dir::audit`).

use apex_test_dir::audit::{offenders, Allowance, REMEDY};
use std::path::Path;

#[test]
fn no_test_spells_its_scratch_directory_by_hand() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("workspace root");
    const ALLOWED: [Allowance; 2] = [
        Allowance {
            path: "crates/apex-examples/examples/prefab_isolated.rs",
            uses: 1,
            why: "the example's prefab file, one fixed name overwritten per run",
        },
        Allowance {
            path: "crates/apex-examples/examples/serialization_hot_reload.rs",
            uses: 2,
            why: "the example's save and config files, fixed names overwritten per run",
        },
    ];
    let found = offenders(&root, &ALLOWED);
    assert!(found.is_empty(), "{REMEDY}\n  {}", found.join("\n  "));
}
