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
    #[error("no services with a flake reference to check")]
    NothingToCheck,
    #[error("service '{0}' was never deployed")]
    NeverDeployed(String),
    #[error("current generation of '{0}' predates diff support; run an update first")]
    NoArtifactRecorded(String),
    #[error("service '{0}' has no older generation to roll back to")]
    NoOlderGeneration(String),
    #[error("settings of '{service}' reference dangling store path {path}")]
    DanglingStorePath { service: String, path: String },
    #[error("{path} has schema version {found}, this flakelet only supports up to {supported}")]
    SchemaTooNew {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("service '{0}' is declared in the host configuration; not overriding it manually")]
    DeclaredService(String),
    #[error("input override '{input}' of '{service}' is not supported; only 'nixpkgs' can be overridden")]
    UnsupportedInputOverride { service: String, input: String },
    #[error("driver evaluation produced no derivation for '{0}'")]
    NoDerivation(String),
    #[error("nix build of {0} produced no output path")]
    NoBuildOutput(String),
    #[error("module of '{0}' produced no units")]
    NoUnits(String),
    #[error(
        "port {port}/{protocol} claimed by '{service}' is already claimed by service '{owner}'"
    )]
    PortConflict {
        service: String,
        port: u64,
        protocol: String,
        owner: String,
    },
    #[error("unit '{unit}' of '{service}' is already managed by service '{owner}'")]
    UnitConflict {
        service: String,
        unit: String,
        owner: String,
    },
    #[error("unit '{unit}' of '{service}' failed after start")]
    UnitFailed { service: String, unit: String },
    #[error("health check {script} of '{service}' failed")]
    HealthCheckFailed { service: String, script: PathBuf },
    #[error("rollback of '{service}' after failed deploy also failed: {source}")]
    RollbackFailed {
        service: String,
        #[source]
        source: Box<Error>,
    },
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
