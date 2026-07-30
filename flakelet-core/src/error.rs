use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{context}: {source}")]
    Json {
        context: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to run {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} {args} failed:\n{stderr}")]
    Command {
        program: String,
        args: String,
        stderr: String,
    },
    #[error("evaluation of {attr} failed: {message}")]
    Eval { attr: String, message: String },
    #[error("lock {path} is held ({holder})")]
    LockHeld { path: PathBuf, holder: String },
    #[error("service '{0}' is not configured")]
    UnknownService(String),
    #[error("settings reference dangling store path {0}")]
    DanglingStorePath(String),
    /// Deployment/consistency errors (no images, health check failure, rollback issues, ...).
    #[error("{0}")]
    Deploy(String),
}

impl Error {
    pub fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }

    pub fn json(context: impl Into<String>) -> impl FnOnce(serde_json::Error) -> Self {
        let context = context.into();
        move |source| Self::Json { context, source }
    }

    /// Whether this looks like a network/substituter problem (offline fallback).
    pub fn is_network_error(&self) -> bool {
        let Self::Command { stderr, .. } = self else {
            return false;
        };
        [
            "unable to download",
            "Couldn't resolve host",
            "Could not resolve host",
            "Network is unreachable",
            "Connection refused",
            "Connection timed out",
            "SSL connection",
        ]
        .iter()
        .any(|pat| stderr.contains(pat))
    }
}
