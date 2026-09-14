//! **A test's scratch directory belongs to a value, and the value removes it.**
//!
//! Before this crate every test that needed a place on disk spelled it by hand:
//! `std::env::temp_dir().join(format!("apex_x_{}", std::process::id()))`, a `remove_dir_all` BEFORE
//! the test (which only ever clears the test's own name - a name the previous run, with another
//! pid, never had) and, sometimes, a `remove_dir_all` as the last line of the body, which a failing
//! assertion skips. Nothing removed anything. Found 2026-09-14 (TD-564): 2030 directories and
//! 25.5 GB of starter projects in `%TEMP%`, one per test per run of the editor suites, filling the
//! disk the builds needed.
//!
//! The class is a resource whose release is a separate line at the end instead of the `Drop` of
//! the value that owns it. So the release is the `Drop` here, as in the reference
//! (`tempfile::TempDir`), and it runs while a panic unwinds too. Two differences from the reference,
//! both on purpose:
//!
//! * **A directory that could not be removed is an error, not silence.** `tempfile` ignores the
//!   result of its removal; on Windows a handle the test left open (a file some background thread
//!   still reads, a watcher) makes the removal fail and the leak comes back without a word. Here the
//!   removal is retried for a bounded time - a handle closed by another thread a moment later is not
//!   a defect - and then the test FAILS, naming the directory and the error. While the test is
//!   already panicking a second panic would abort the process, so then it is printed.
//! * **Every directory lives under one parent**, `<temp>/apex_test_dirs/`, so what a run leaves
//!   behind is one place to look at, and the engine's `tools/sweep_stale_builds.py` knows where.
//!
//! `APEX_KEEP_TEST_DIRS=1` keeps the directories (and prints where) - for looking at what a test
//! wrote. It is the only way to keep one: a kept directory is exactly the leak this crate exists to
//! stop, so it is never the default, not even for a failing test.
//!
//! It lives in the CORE because both repositories write tests to disk and a rule kept in two copies
//! is obeyed in one: the engine takes it by path like every other core crate. [`audit`] is the
//! source rule that keeps it the only road - `std::env::temp_dir()` outside this crate and a
//! workspace's short list of production uses is refused - read by this crate's own
//! `workspace_hygiene` gate and by the engine's `source_hygiene`.

pub mod audit;

use std::ffi::OsStr;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The one parent every test directory is made in.
pub const PARENT: &str = "apex_test_dirs";

/// The environment knob that keeps the directories instead of removing them.
pub const KEEP_ENV: &str = "APEX_KEEP_TEST_DIRS";

/// How long a failed removal is retried before it is an error. A removal that is going to succeed
/// succeeds on the first try; this bounds the case where another thread is still closing a handle
/// into the directory a moment after the test's own values were dropped.
const REMOVE_DEADLINE: Duration = Duration::from_secs(5);

/// A directory that exists from [`TestDir::new`] until this value is dropped.
///
/// It dereferences to [`Path`], so `dir.join("assets")`, `&*dir` and `dir.display()` read as they
/// did on a `PathBuf`, and `&dir` converts into a `PathBuf` where an API takes `impl Into<PathBuf>`.
/// Keep the value bound for as long as anything uses the path: `TestDir::new("x").join("y")`
/// removes the directory at the end of that statement (the source gate refuses the shape).
#[must_use = "the directory is removed when this value is dropped - bind it for as long as the path is used"]
pub struct TestDir {
    path: PathBuf,
    /// [`REMOVE_DEADLINE`]; shorter only in this crate's own test of the failure.
    deadline: Duration,
}

impl TestDir {
    /// Creates a fresh, empty directory `<temp>/apex_test_dirs/<label>_<pid>_<n>`.
    ///
    /// The name is never reused: it is created with `create_dir` (not `create_dir_all`), and a name
    /// that already exists - left by a process that was killed before its drops ran and happened to
    /// get the same pid - moves on to the next number instead of deleting somebody's directory.
    ///
    /// `label` names the test in the path; letters, digits, `_` and `-` only, so it cannot step
    /// outside the parent.
    pub fn new(label: impl AsRef<str>) -> TestDir {
        let label = label.as_ref();
        assert!(
            !label.is_empty()
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "a test directory label is letters, digits, `_` and `-` only, and not empty: {label:?}"
        );
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = std::env::temp_dir().join(PARENT);
        if let Err(e) = std::fs::create_dir_all(&parent) {
            panic!("cannot create the test directory parent {}: {e}", parent.display());
        }
        let pid = std::process::id();
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("{label}_{pid}_{n}"));
            match std::fs::create_dir(&path) {
                Ok(()) => return TestDir { path, deadline: REMOVE_DEADLINE },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("cannot create the test directory {}: {e}", path.display()),
            }
        }
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// Lets `&dir` go where an API takes `impl Into<PathBuf>` (`PathBuf: From<&T> where T: AsRef<OsStr>`).
impl AsRef<OsStr> for TestDir {
    fn as_ref(&self) -> &OsStr {
        self.path.as_os_str()
    }
}

impl std::fmt::Debug for TestDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.path.fmt(f)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if std::env::var(KEEP_ENV).is_ok_and(|v| !v.is_empty() && v != "0") {
            eprintln!("{KEEP_ENV}: kept {}", self.path.display());
            return;
        }
        let Err(e) = remove_with_retry(&self.path, self.deadline) else {
            return;
        };
        let message = format!(
            concat!(
                "the test directory {} could not be removed: {}. Something the test made still holds ",
                "a handle into it after the test ended (a file left open, a thread that outlived its ",
                "owner). The directory stays on disk - that is the leak TD-564 closed."
            ),
            self.path.display(),
            e
        );
        if std::thread::panicking() {
            eprintln!("{message}");
        } else {
            panic!("{message}");
        }
    }
}

/// `remove_dir_all`, retried until `deadline` while it fails with anything but "already gone".
fn remove_with_retry(path: &Path, deadline: Duration) -> std::io::Result<()> {
    let start = Instant::now();
    let mut pause = Duration::from_millis(5);
    loop {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if start.elapsed() >= deadline => return Err(e),
            Err(_) => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(200));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_exists_while_its_value_lives_and_is_gone_after() {
        let dir = TestDir::new("test_dir_lifetime");
        assert!(dir.is_dir(), "the directory exists from `new`");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/file.txt"), b"x").unwrap();
        let path = dir.to_path_buf();
        drop(dir);
        assert!(!path.exists(), "dropping the value removes the directory and what is in it");
    }

    #[test]
    fn a_panicking_test_removes_its_directory_too() {
        let path = std::sync::Mutex::new(None);
        let result = std::panic::catch_unwind(|| {
            let dir = TestDir::new("test_dir_panic");
            std::fs::write(dir.join("file.txt"), b"x").unwrap();
            *path.lock().unwrap() = Some(dir.to_path_buf());
            panic!("the test body fails");
        });
        assert!(result.is_err());
        let path = path.into_inner().unwrap().expect("the body ran");
        assert!(!path.exists(), "the unwind ran the drop: {}", path.display());
    }

    #[test]
    fn two_directories_with_one_label_are_two_directories() {
        let a = TestDir::new("test_dir_twice");
        let b = TestDir::new("test_dir_twice");
        assert_ne!(a.path(), b.path());
        assert!(a.is_dir() && b.is_dir());
    }

    #[test]
    fn a_name_left_on_disk_by_another_run_is_skipped_not_deleted() {
        // What a killed process with a reused pid would have left: the next names of this process.
        let probe = TestDir::new("test_dir_left");
        let parent = probe.parent().unwrap().to_path_buf();
        let pid = std::process::id();
        let n: u64 = probe.file_name().unwrap().to_str().unwrap().rsplit('_').next().unwrap().parse().unwrap();
        // The counter is shared with the tests running beside this one, so a band of names is squatted
        // and only the ones this test actually created are its evidence.
        let squatters: Vec<PathBuf> = (n + 1..n + 65)
            .map(|k| parent.join(format!("test_dir_left_{pid}_{k}")))
            .filter(|p| std::fs::create_dir(p).is_ok())
            .collect();
        assert!(!squatters.is_empty(), "the stand squatted no name at all");
        for s in &squatters {
            std::fs::write(s.join("theirs.txt"), b"x").unwrap();
        }
        let mine = TestDir::new("test_dir_left");
        assert!(!squatters.iter().any(|s| s == mine.path()), "a name on disk was handed out again");
        drop(mine);
        for s in &squatters {
            assert!(s.join("theirs.txt").is_file(), "another run's directory is not touched");
            std::fs::remove_dir_all(s).unwrap();
        }
    }

    #[test]
    fn a_path_api_takes_the_directory_as_it_took_a_path_buf() {
        fn takes_into(p: impl Into<PathBuf>) -> PathBuf {
            p.into()
        }
        fn takes_as_ref(p: impl AsRef<Path>) -> PathBuf {
            p.as_ref().to_path_buf()
        }
        let dir = TestDir::new("test_dir_api");
        assert_eq!(takes_into(&dir), dir.to_path_buf());
        assert_eq!(takes_as_ref(&dir), dir.to_path_buf());
    }

    /// The failure is loud: a directory something still holds open fails the test that owned it.
    /// Windows only, because that is where an open handle blocks the removal (and where the leak
    /// was); elsewhere unlinking an open file succeeds and there is nothing to fail.
    #[cfg(windows)]
    #[test]
    fn a_directory_that_cannot_be_removed_fails_the_test_and_says_where() {
        use std::os::windows::fs::OpenOptionsExt;
        let mut dir = TestDir::new("test_dir_held");
        dir.deadline = Duration::from_millis(50);
        let path = dir.to_path_buf();
        // No FILE_SHARE_DELETE: exactly what a reader thread that outlived its test holds.
        let held = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .share_mode(0x1 | 0x2)
            .open(path.join("held.bin"))
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(dir)));
        let message = result.expect_err("a removal that failed must fail the owner");
        let message = message
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(
            message.contains("could not be removed") && message.contains(&*path.to_string_lossy()),
            "the failure names the directory: {message}"
        );
        drop(held);
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    #[should_panic(expected = "letters, digits")]
    fn a_label_with_a_separator_is_refused() {
        let _ = TestDir::new("../escape");
    }
}
