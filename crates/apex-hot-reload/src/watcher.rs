//! FileWatcher — обёртка над `notify` с каналом изменений.

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    time::Duration,
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
pub struct FileWatcher {
    _watcher: RecommendedWatcher, // держим живым
    rx:       Receiver<FileChange>,
}

impl FileWatcher {
    /// Создать watcher для директории `watch_dir`.
    ///
    /// Рекурсивно следит за всеми файлами внутри.
    /// События дебаунсируются на `debounce` — типично 50–200ms.
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
            Config::default().with_poll_interval(debounce),
        )?;

        watcher.watch(watch_dir, RecursiveMode::Recursive)?;

        Ok(Self { _watcher: watcher, rx })
    }

    /// Опросить все накопившиеся события без блокировки.
    ///
    /// Возвращает `Vec<FileChange>` — может быть пустым если изменений нет.
    /// Вызывается каждый кадр из game loop.
    pub fn poll(&self) -> Vec<FileChange> {
        let mut changes = Vec::new();
        while let Ok(change) = self.rx.try_recv() {
            changes.push(change);
        }
        // Дедупликация — notify может слать несколько событий для одного файла
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes.dedup_by(|a, b| a.path == b.path);
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