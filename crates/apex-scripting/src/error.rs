//! Типы ошибок apex-scripting.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ScriptError {
    #[error("Ошибка компиляции скрипта '{name}': {source}")]
    Compile {
        name:   String,
        #[source]
        source: Box<rhai::ParseError>,
    },

    #[error("Ошибка выполнения скрипта '{name}': {source}")]
    Runtime {
        name:   String,
        #[source]
        source: Box<rhai::EvalAltResult>,
    },

    #[error("Скрипт '{0}' не найден")]
    NotFound(String),

    #[error("Ошибка чтения файла '{path}': {source}")]
    Io {
        path:   String,
        #[source]
        source: std::io::Error,
    },

    #[error("Ошибка файлового наблюдателя: {0}")]
    Watcher(String),

    #[error("Директория скриптов не задана")]
    NoScriptDir,
}

impl ScriptError {
    pub fn compile(name: impl Into<String>, e: rhai::ParseError) -> Self {
        Self::Compile { name: name.into(), source: Box::new(e) }
    }

    pub fn runtime(name: impl Into<String>, e: Box<rhai::EvalAltResult>) -> Self {
        Self::Runtime { name: name.into(), source: e }
    }

    pub fn io(path: impl Into<String>, e: std::io::Error) -> Self {
        Self::Io { path: path.into(), source: e }
    }
}