//! apex-scripting error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ScriptError {
    #[error("Failed to compile script '{name}': {source}")]
    Compile {
        name:   String,
        #[source]
        source: mlua::Error,
    },

    #[error("Failed to run script '{name}': {source}")]
    Runtime {
        name:   String,
        #[source]
        source: mlua::Error,
    },

    #[error("Script '{0}' not found")]
    NotFound(String),

    #[error("Failed to read file '{path}': {source}")]
    Io {
        path:   String,
        #[source]
        source: std::io::Error,
    },

    #[error("File watcher error: {0}")]
    Watcher(String),

    #[error("Scripts directory is not set")]
    NoScriptDir,
}

impl ScriptError {
    pub fn compile(name: impl Into<String>, e: mlua::Error) -> Self {
        Self::Compile { name: name.into(), source: e }
    }

    pub fn runtime(name: impl Into<String>, e: mlua::Error) -> Self {
        Self::Runtime { name: name.into(), source: e }
    }

    pub fn io(path: impl Into<String>, e: std::io::Error) -> Self {
        Self::Io { path: path.into(), source: e }
    }
}
