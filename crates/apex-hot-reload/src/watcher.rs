//! FileWatcher — обёртка над `notify` с каналом изменений.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Изменение файла — приходит из background watcher thread.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
}

/// FileWatcher — запускает notify в отдельном потоке, отдаёт изменения через channel.
///
/// # Thread model
/// `notify` запускает внутренний OS-watcher поток. События приходят в `Sender`
/// и читаются из `Receiver` в game loop через `poll()` — без блокировки.
///
/// # Debounce (9b)
/// `notify`'s `Config::with_poll_interval` only affects the polling backend
/// (`PollWatcher`); the native backends — `ReadDirectoryChangesW` on Windows,
/// `inotify` on Linux, `FSEvents`/`kqueue` on macOS — ignore it, so there is no
/// real debounce there. A save often arrives as several events while an editor
/// is still writing the file, and reading it mid-write yields a truncated or
/// invalid file.
///
/// We therefore debounce **manually** in [`poll`](FileWatcher::poll): each raw
/// event marks its path as *pending* with the time it was first seen, and a
/// path is only emitted once `now >= first_seen + debounce` — i.e. once it has
/// stayed quiet for the debounce window. Re-touching a still-pending path
/// pushes its deadline back, coalescing a burst of writes into a single change.
pub struct FileWatcher {
    _watcher: RecommendedWatcher, // держим живым
    rx:       Receiver<FileChange>,
    debounce: Duration,
    /// Paths seen but not yet settled: path → deadline (`first_seen + debounce`).
    /// A path is emitted by `poll` only once `now` reaches its deadline.
    pending:  HashMap<PathBuf, Instant>,
}

impl FileWatcher {
    /// Создать watcher для директории `watch_dir`.
    ///
    /// Рекурсивно следит за всеми файлами внутри.
    /// События дебаунсируются на `debounce` — типично 50–200ms (см. `poll`).
    pub fn new(watch_dir: &Path, debounce: Duration) -> Result<Self, notify::Error> {
        let (tx, rx) = mpsc::channel::<FileChange>();
        let tx_clone = tx.clone();

        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                if let Ok(event) = res {
                    // Нас интересуют только события изменения содержимого файла
                    match event.kind {
                        EventKind::Modify(_) | EventKind::Create(_) => {
                            for path in event.paths {
                                // Игнорируем временные файлы редакторов
                                if Self::is_temp_file(&path) { continue; }
                                let _ = tx_clone.send(FileChange { path });
                            }
                        }
                        _ => {}
                    }
                }
            },
            // Kept for the polling backend; native backends ignore it, which is
            // exactly why we debounce manually in `poll` (see type docs / 9b).
            Config::default().with_poll_interval(debounce),
        )?;

        watcher.watch(watch_dir, RecursiveMode::Recursive)?;

        Ok(Self {
            _watcher: watcher,
            rx,
            debounce,
            pending: HashMap::new(),
        })
    }

    /// Опросить все накопившиеся события без блокировки.
    ///
    /// Возвращает `Vec<FileChange>` — может быть пустым если изменений нет.
    /// Вызывается каждый кадр из game loop.
    ///
    /// Applies the manual debounce (9b): raw events are folded into the pending
    /// map and only paths that have stayed quiet for `debounce` are emitted.
    pub fn poll(&mut self) -> Vec<FileChange> {
        self.poll_at(Instant::now())
    }

    /// [`poll`](FileWatcher::poll) with an injected `now` — the debounce logic
    /// lives here so it can be tested deterministically without sleeping.
    ///
    /// Drains the channel first (recording/refreshing pending deadlines), then
    /// returns every path whose deadline has passed and clears it from pending.
    pub(crate) fn poll_at(&mut self, now: Instant) -> Vec<FileChange> {
        // 1. Drain raw events into the pending map, (re)arming each deadline.
        while let Ok(change) = self.rx.try_recv() {
            self.pending.insert(change.path, now + self.debounce);
        }

        // 2. Emit only settled paths (deadline reached), removing them.
        let ready: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(_, &deadline)| now >= deadline)
            .map(|(path, _)| path.clone())
            .collect();

        let mut changes: Vec<FileChange> = Vec::with_capacity(ready.len());
        for path in ready {
            self.pending.remove(&path);
            changes.push(FileChange { path });
        }

        // Стабильный порядок для детерминизма (map-итерация неупорядочена).
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
    }

    /// Временные файлы редакторов которые не нужно перезагружать.
    fn is_temp_file(path: &Path) -> bool {
        let name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        // vim swap, emacs backup, JetBrains temp, Windows lock
        name.starts_with('.')
            || name.ends_with('~')
            || name.ends_with(".swp")
            || name.ends_with(".tmp")
            || name.ends_with(".bak")
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Build a `FileWatcher` whose channel we control directly, bypassing the
    /// OS backend. Only the debounce logic (`poll_at`) is exercised here.
    ///
    /// SAFETY/scope: we still need a real `RecommendedWatcher` to fill the
    /// `_watcher` field, so we create one over a temp dir. The events we test
    /// with are injected through the returned `Sender`, not the OS.
    fn test_watcher(debounce: Duration) -> (FileWatcher, mpsc::Sender<FileChange>) {
        let dir = std::env::temp_dir().join("apex_watcher_debounce_test");
        let _ = std::fs::create_dir_all(&dir);

        let (tx, rx) = mpsc::channel::<FileChange>();
        // A real watcher just to own the backend thread; it won't feed `rx`.
        let watcher = RecommendedWatcher::new(
            |_res: notify::Result<Event>| {},
            Config::default(),
        )
        .expect("create watcher");

        let fw = FileWatcher {
            _watcher: watcher,
            rx,
            debounce,
            pending: HashMap::new(),
        };
        (fw, tx)
    }

    #[test]
    fn debounce_holds_path_until_window_elapses() {
        let debounce = Duration::from_millis(100);
        let (mut fw, tx) = test_watcher(debounce);

        let t0 = Instant::now();
        let p = PathBuf::from("config.json");
        tx.send(FileChange { path: p.clone() }).unwrap();

        // Same instant the event arrived: not yet settled → nothing emitted.
        let out = fw.poll_at(t0);
        assert!(out.is_empty(), "path emitted before debounce elapsed");

        // Half-way through the window: still pending.
        let out = fw.poll_at(t0 + Duration::from_millis(50));
        assert!(out.is_empty(), "path emitted mid-debounce");

        // After the window: emitted exactly once.
        let out = fw.poll_at(t0 + Duration::from_millis(100));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, p);

        // And it is cleared afterwards — not re-emitted.
        let out = fw.poll_at(t0 + Duration::from_millis(200));
        assert!(out.is_empty(), "settled path re-emitted");
    }

    #[test]
    fn debounce_coalesces_burst_and_pushes_deadline_back() {
        let debounce = Duration::from_millis(100);
        let (mut fw, tx) = test_watcher(debounce);

        let t0 = Instant::now();
        let p = PathBuf::from("scene.prefab.json");

        // A burst of writes while the editor is still saving.
        tx.send(FileChange { path: p.clone() }).unwrap();
        let _ = fw.poll_at(t0); // arms deadline = t0 + 100ms

        // New write at t0+80ms re-arms deadline to t0+180ms.
        tx.send(FileChange { path: p.clone() }).unwrap();
        let out = fw.poll_at(t0 + Duration::from_millis(80));
        assert!(out.is_empty(), "coalesced burst emitted too early");

        // At t0+150ms the ORIGINAL window has passed but the refreshed one has
        // not — must still be pending (debounce follows the latest write).
        let out = fw.poll_at(t0 + Duration::from_millis(150));
        assert!(out.is_empty(), "deadline was not pushed back by later write");

        // Once quiet past the refreshed deadline: a single coalesced event.
        let out = fw.poll_at(t0 + Duration::from_millis(181));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, p);
    }
}
