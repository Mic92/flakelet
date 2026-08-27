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
    #[error("no services with a flake reference")]
    NothingToCheck,
    #[error("output '{output}' of '{service}' is not a dotted attribute path")]
    InvalidOutput { service: String, output: String },
    #[error("invalid service name '{0}': use letters, digits, '-' or '_'")]
    InvalidServiceName(String),
    #[error("service '{0}' was never deployed")]
    NeverDeployed(String),
    #[error("service '{service}' is disabled ({reason}), run 'flakelet enable {service}' first")]
    Disabled { service: String, reason: String },
    #[error("service '{0}' has no older generation to roll back to")]
    NoOlderGeneration(String),
    #[error("settings of '{service}' reference dangling store path {path}")]
    DanglingStorePath { service: String, path: String },
    #[error("prebuilt artifact {0} does not exist")]
    MissingArtifact(PathBuf),
    #[error("{path} has schema version {found}, this flakelet only supports up to {supported}")]
    SchemaTooNew {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("service '{0}' is declared in the host configuration. Use 'flakelet update {0} --flake <ref>' to test another ref")]
    DeclaredService(String),
    #[error("service '{0}' uses a prebuilt artifact. --flake has no effect")]
    OverrideWithPrebuilt(String),
    #[error("input override '{input}' of '{service}' is not supported; only 'nixpkgs' can be overridden")]
    UnsupportedInputOverride { service: String, input: String },
    #[error("the config requires flakelet_lib and adios (module validation and the korora type checker)")]
    LibRequiresAdios,
    #[error("credential path {0} contains whitespace or quotes")]
    UnsafeCredentialPath(PathBuf),
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
    #[error("unit '{unit}' of '{service}' must be named '{service}.<type>' or '{service}-*.<type>' with type one of service, socket, target, timer, path")]
    InvalidUnitName { service: String, unit: String },
    #[error("unit '{unit}' of '{service}' would shadow the host's unit at {path}")]
    HostUnitConflict {
        service: String,
        unit: String,
        path: PathBuf,
    },
    #[error("unit '{unit}' of '{service}' is already managed by service '{owner}'")]
    UnitConflict {
        service: String,
        unit: String,
        owner: String,
    },
    #[error("unit '{unit}' of '{service}' failed after start")]
    UnitFailed { service: String, unit: String },
    #[error("health probe unit '{unit}' of '{service}' failed")]
    HealthCheckFailed { service: String, unit: String },
    #[error("service '{service}' cannot be {verb}:\n  {}", reasons.join("\n  "))]
    NotTransferable {
        service: String,
        verb: &'static str,
        reasons: Vec<String>,
    },
    #[error("oneshot unit '{unit}' of '{service}' failed")]
    OneshotFailed { service: String, unit: String },
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
