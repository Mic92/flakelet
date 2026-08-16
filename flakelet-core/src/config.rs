use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;

/// Rendered by the NixOS module to /etc/flakelet/config.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub eval_user: Option<String>,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
    pub gcroot_dir: PathBuf,
    /// Volatile runtime data (published exports), cleared on reboot.
    pub runtime_dir: PathBuf,
    /// Provider capability announcements (`<contract>.json` files).
    pub providers_dir: PathBuf,
    /// Store path of the host's nixpkgs source, imported once by the driver.
    pub nixpkgs: Option<PathBuf>,
    /// Store path of the adios library source, injected into service modules.
    pub adios: Option<PathBuf>,
    /// Store path of flakelet.lib (mkService, storePath), injected into
    /// service modules as `flakeletLib`.
    pub flakelet_lib: Option<PathBuf>,
    /// Extra host-provided helper modules passed to service functions.
    pub extra_modules: Vec<PathBuf>,
    pub eval: EvalSettings,
    pub credentials: Option<Credentials>,
    pub services: BTreeMap<String, ServiceConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            eval_user: None,
            cache_dir: "/var/cache/flakelet".into(),
            state_dir: "/var/lib/flakelet".into(),
            gcroot_dir: "/nix/var/nix/gcroots/flakelet".into(),
            runtime_dir: "/run/flakelet".into(),
            providers_dir: "/etc/flakelet/providers.d".into(),
            nixpkgs: None,
            adios: None,
            flakelet_lib: None,
            extra_modules: Vec::new(),
            eval: EvalSettings::default(),
            credentials: None,
            services: BTreeMap::new(),
        }
    }
}

/// Resource limits for nix-eval-jobs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EvalSettings {
    /// Worker count (default 1: the batch shares one nixpkgs instance).
    pub workers: Option<u32>,
    /// Restart a worker above this many MiB. Default: derived from available RAM.
    pub max_memory_mb: Option<u64>,
}

/// Fetch credentials for private flakes; file paths readable by the eval user.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    pub netrc_file: Option<PathBuf>,
    /// File with `host=token` lines, passed to nix via NIX_CONFIG access-tokens.
    pub access_tokens_file: Option<PathBuf>,
    pub ssh_key_file: Option<PathBuf>,
    pub ssh_known_hosts_file: Option<PathBuf>,
}

/// One service. Declarative services live in config.json,
/// manually deployed ones in <state_dir>/<name>/service.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceConfig {
    pub flake: String,
    pub output: String,
    /// Host settings, embedded into the driver expression as Nix values.
    pub settings: Value,
    /// Store path of an already built driver output (units/, exports.json).
    /// Skips resolution, evaluation and build.
    pub prebuilt: Option<PathBuf>,
    pub input_overrides: BTreeMap<String, String>,
    pub keep_generations: u32,
    /// Per-service override of the global credentials block.
    pub credentials: Option<Credentials>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            flake: String::new(),
            output: "flakelets.default".into(),
            settings: Value::Object(Default::default()),
            prebuilt: None,
            input_overrides: BTreeMap::new(),
            keep_generations: 5,
            credentials: None,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        // A missing config file just means: no declarative services.
        let cfg: Self = match fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data)
                .map_err(Error::json(format!("cannot parse {}", path.display())))?,
            Err(e) if e.kind() == ErrorKind::NotFound => Self::default(),
            Err(source) => {
                return Err(Error::Io {
                    context: format!("cannot read config {}", path.display()),
                    source,
                })
            }
        };
        // The driver validates modules with flakelet.lib, which imports
        // korora from the adios source tree; fail early instead of deep
        // inside an evaluation.
        if cfg.flakelet_lib.is_none() || cfg.adios.is_none() {
            return Err(Error::LibRequiresAdios);
        }
        if cfg.version > SCHEMA_VERSION {
            return Err(Error::SchemaTooNew {
                path: path.into(),
                found: cfg.version,
                supported: SCHEMA_VERSION,
            });
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config() {
        let json = r#"{
          "eval_user": "flakelet",
          "nixpkgs": "/nix/store/aaa-source",
          "adios": "/nix/store/bbb-adios",
          "eval": { "workers": 2 },
          "credentials": { "netrc_file": "/run/secrets/netrc" },
          "services": {
            "grafana": { "flake": "github:me/grafana-svc" },
            "svc": {
              "flake": "git+https://example.com/svc",
              "settings": { "port": 8080, "cert": "/nix/store/ccc-cert.pem" },
              "input_overrides": { "nixpkgs": "github:NixOS/nixpkgs/nixos-25.05" },
              "keep_generations": 2
            }
          }
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.eval.workers, Some(2));
        assert_eq!(cfg.services["grafana"].output, "flakelets.default");
        assert_eq!(cfg.services["grafana"].keep_generations, 5);
        assert_eq!(cfg.services["svc"].settings["port"], 8080);
    }
}
