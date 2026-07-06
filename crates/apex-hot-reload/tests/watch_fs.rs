//! End-to-end file-watcher tests against the real OS backend.
//!
//! The inline tests in `watcher.rs` drive the debounce logic through the
//! crate-private `poll_at`, injecting events directly — they never touch the
//! filesystem. These tests create a real [`FileWatcher`] over a temp directory,
//! write real files, and assert the change surfaces through the public `poll()`
//! (and that editor temp files are filtered out). They tolerate OS event
//! latency with a bounded retry loop rather than a single fixed sleep.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use apex_hot_reload::FileWatcher;

/// A short debounce so the test settles quickly, but non-zero so we still go
/// through the real coalescing path.
const DEBOUNCE: Duration = Duration::from_millis(30);
/// Generous ceiling for a native watcher to deliver an event on a busy machine.
const TIMEOUT: Duration = Duration::from_secs(5);

/// A unique, clean temp directory for one test (tests share a process, so the
/// name is disambiguated per-test to avoid cross-talk).
fn fresh_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("apex_watch_fs_{}_{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp watch dir");
    dir
}

/// Poll until a file whose name matches `wanted` surfaces, or the timeout hits.
/// Returns the set of file names seen along the way (for negative assertions).
fn wait_for(fw: &mut FileWatcher, wanted: &str, timeout: Duration) -> (bool, Vec<String>) {
    let start = Instant::now();
    let mut seen = Vec::new();
    while start.elapsed() < timeout {
        for change in fw.poll() {
            if let Some(name) = change.path.file_name().and_then(|n| n.to_str()) {
                seen.push(name.to_string());
                if name == wanted {
                    return (true, seen);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    (false, seen)
}

fn write(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write test file");
}

#[test]
fn real_watch_detects_a_written_file() {
    let dir = fresh_dir("detect");
    let mut fw = FileWatcher::new(&dir, DEBOUNCE).expect("create watcher");

    // Let the OS backend finish arming before the first write.
    std::thread::sleep(Duration::from_millis(150));
    write(&dir, "asset.json", "{ \"v\": 1 }");

    let (found, seen) = wait_for(&mut fw, "asset.json", TIMEOUT);
    assert!(
        found,
        "a real file write must surface through poll() (saw: {seen:?})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn real_watch_filters_editor_temp_files() {
    let dir = fresh_dir("filter");
    let mut fw = FileWatcher::new(&dir, DEBOUNCE).expect("create watcher");

    std::thread::sleep(Duration::from_millis(150));
    // A temp file that must be filtered, then a real one that must surface.
    write(&dir, "scene.json.tmp", "partial");
    write(&dir, "scene.json", "{ \"ok\": true }");

    let (found, seen) = wait_for(&mut fw, "scene.json", TIMEOUT);
    assert!(found, "the real file must surface (saw: {seen:?})");
    assert!(
        !seen.iter().any(|n| n.ends_with(".tmp")),
        "editor temp files must never surface (saw: {seen:?})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
