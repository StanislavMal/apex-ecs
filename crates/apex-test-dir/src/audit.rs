//! **The source rule that keeps a test's scratch directory a value** — read from the text of a
//! workspace, so it costs no build of the code it reads.
//!
//! One implementation with two readers: the core's own gate (`tests/workspace_hygiene.rs` of this
//! crate) and the engine's `source_hygiene` (`apex-editor-macros`). A rule written once per
//! repository is obeyed in one of them (lesson 60), so the shapes live here and each repository
//! only states its ALLOWANCES.
//!
//! Two shapes are refused:
//!
//! * **`std::env::temp_dir()` in code**, outside this crate and outside a workspace's allowance list.
//!   Every allowance names its file, its exact number of uses and why the use is not a test's
//!   scratch directory - so a second use in an allowed file is refused as well as a first one in a
//!   new file.
//! * **A `TestDir` that dies in its own statement**: `TestDir::new(..).join(..)` (the temporary is
//!   dropped at the `;`) or `let _ = TestDir::new(..)` (`_` does not bind). The directory is removed
//!   before the path is used, and whatever writes to the path afterwards recreates it where nothing
//!   removes it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A production use of the temporary directory a workspace keeps on purpose.
#[derive(Clone, Copy, Debug)]
pub struct Allowance {
    /// The file, relative to the workspace root, with `/` separators.
    pub path: &'static str,
    /// How many code lines of that file name `std::env::temp_dir()`.
    pub uses: usize,
    /// Why this is not a test's scratch directory.
    pub why: &'static str,
}

/// What to tell the author when [`offenders`] is not empty.
pub const REMEDY: &str = concat!(
    "a scratch directory is spelled by hand, or dies before it is used - take ",
    "`let dir = apex_test_dir::TestDir::new(\"label\");` and keep `dir` bound while the path is in ",
    "use: its Drop removes the directory, also when the test panics (engine TD-564). A production use ",
    "of the temporary directory goes into the workspace's allowance list with its reason."
);

/// Every refused shape in the `.rs` files under `root`, as `path:line: what`. Also reports each
/// allowance whose count no longer matches. Build output (`target`), `.git` and directories
/// starting with `_` (read-only reference checkouts) are not the workspace's code.
pub fn offenders(root: &Path, allowed: &[Allowance]) -> Vec<String> {
    let mut files = Vec::new();
    rust_sources(root, &mut files);
    let own_crate = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")));

    let mut out = Vec::new();
    if files.is_empty() {
        out.push(format!("{}: no Rust sources found - the scan looks in the wrong place", root.display()));
        return out;
    }
    let mut counts = HashMap::<String, usize>::new();
    for path in &files {
        // This crate names the shapes it refuses, and owns the one legal use.
        if path.canonicalize().is_ok_and(|p| p.starts_with(&own_crate)) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        // Prose is what the doc comments are made of; only code counts.
        let code: String = text
            .lines()
            .map(|l| if l.trim_start().starts_with("//") { "" } else { l })
            .collect::<Vec<_>>()
            .join("\n");
        for (i, line) in code.lines().enumerate() {
            if line.contains("std::env::temp_dir()") {
                *counts.entry(rel.clone()).or_default() += 1;
                if !allowed.iter().any(|a| a.path == rel) {
                    out.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                }
            }
        }
        out.extend(dropped_in_own_statement(&rel, &code));
    }
    for a in allowed {
        let found = counts.get(a.path).copied().unwrap_or(0);
        if found != a.uses {
            out.push(format!(
                "{}: {found} uses of std::env::temp_dir(), the allowance is {} ({})",
                a.path, a.uses, a.why
            ));
        }
    }
    out
}

/// `TestDir::new(<args>)` followed by `.`, or bound to `_`.
fn dropped_in_own_statement(rel: &str, code: &str) -> Vec<String> {
    const CALL: &str = "TestDir::new(";
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = code[from..].find(CALL) {
        let start = from + at;
        let open = start + CALL.len() - 1;
        let mut depth = 0usize;
        let mut end = code.len();
        for (k, ch) in code[open..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + k + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        let after = code[end..].trim_start();
        let statement_head = code[..start].rsplit(['\n', ';', '{']).next().unwrap_or("");
        if after.starts_with('.') || statement_head.trim_start().starts_with("let _ =") {
            let line = code[..start].matches('\n').count() + 1;
            out.push(format!(
                "{rel}:{line}: a TestDir dropped in its own statement: {}",
                code[start..end].trim()
            ));
        }
        from = end.max(start + 1);
    }
    out
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if p.is_dir() {
            if name == "target" || name == ".git" || name.starts_with('_') {
                continue;
            }
            rust_sources(&p, out);
        } else if name.ends_with(".rs") {
            out.push(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four shapes the scan exists for, planted in a workspace of its own - a reading of the
    /// rule that did not refuse them would be a gate that passes on everything.
    #[test]
    fn every_refused_shape_is_named_and_the_legal_ones_are_not() {
        let ws = crate::TestDir::new("audit_planted");
        let src = ws.join("crates/demo/src");
        std::fs::create_dir_all(&src).unwrap();
        let raw = ["let d = std::env::temp_", "dir().join(\"x\");"].concat();
        let chained = ["let p = apex_test_dir::Test", "Dir::new(\"a\")\n        .join(\"b\");"].concat();
        let underscore = ["let _ = apex_test_dir::Test", "Dir::new(\"c\");"].concat();
        let bound = ["let dir = apex_test_dir::Test", "Dir::new(\"d\");\n    let p = dir.join(\"e\");"].concat();
        let prose = ["// std::env::temp_", "dir() in a comment is prose"].concat();
        std::fs::write(
            src.join("lib.rs"),
            format!("fn f() {{\n    {raw}\n    {chained}\n    {underscore}\n    {bound}\n    {prose}\n}}\n"),
        )
        .unwrap();
        let allowed_file = ["fn g() { let _log = std::env::temp_", "dir(); }\n"].concat();
        std::fs::write(src.join("allowed.rs"), &allowed_file).unwrap();

        let found = offenders(
            ws.path(),
            &[
                Allowance { path: "crates/demo/src/allowed.rs", uses: 1, why: "planted" },
                Allowance { path: "crates/demo/src/gone.rs", uses: 1, why: "a file that no longer uses it" },
            ],
        );
        let has = |needle: &str| found.iter().any(|f| f.contains(needle));
        assert!(has("crates/demo/src/lib.rs:2:"), "the raw call: {found:#?}");
        assert!(has("lib.rs:3: a TestDir dropped"), "the chained temporary: {found:#?}");
        assert!(has("lib.rs:5: a TestDir dropped"), "the `_` binding: {found:#?}");
        assert!(has("gone.rs: 0 uses"), "a stale allowance: {found:#?}");
        assert_eq!(found.len(), 4, "the bound value, the comment and the allowed use are legal: {found:#?}");
    }
}
