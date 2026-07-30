use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Rendered by the NixOS module to /etc/flakelet/config.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub eval_user: Option<String>,
    #[serde(default = "d_cache_dir")]
    pub cache_dir: PathBuf,
    #[serde(default = "d_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "d_gcroot_dir")]
    pub gcroot_dir: PathBuf,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceConfig>,
}

/// One portable service. Declarative services live in config.json,
/// manually deployed ones in <state_dir>/<name>/service.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub flake: String,
    #[serde(default = "d_output")]
    pub output: String,
    #[serde(default)]
    pub settings_file: Option<PathBuf>,
    #[serde(default = "d_profile")]
    pub profile: String,
    #[serde(default)]
    pub extra_portablectl_args: Vec<String>,
    #[serde(default)]
    pub input_overrides: BTreeMap<String, String>,
    #[serde(default)]
    pub health_check: HealthCheck,
    #[serde(default = "d_keep")]
    pub keep_generations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    /// Seconds to wait after attaching before checking unit state.
    #[serde(default = "d_timeout")]
    pub timeout: u64,
    /// Non-zero exit means unhealthy.
    #[serde(default)]
    pub command: Option<String>,
}

impl Default for HealthCheck {
    fn default() -> Self {
        Self {
            timeout: d_timeout(),
            command: None,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            eval_user: None,
            cache_dir: d_cache_dir(),
            state_dir: d_state_dir(),
            gcroot_dir: d_gcroot_dir(),
            services: BTreeMap::new(),
        }
    }
}

fn d_cache_dir() -> PathBuf {
    "/var/cache/flakelet".into()
}
fn d_state_dir() -> PathBuf {
    "/var/lib/flakelet".into()
}
fn d_gcroot_dir() -> PathBuf {
    "/nix/var/nix/gcroots/flakelet".into()
}
fn d_profile() -> String {
    "default".into()
}
fn d_keep() -> u32 {
    5
}
fn d_timeout() -> u64 {
    30
}
fn d_output() -> String {
    format!(
        "portableServices.{}-{}.default",
        std::env::consts::ARCH,
        std::env::consts::OS
    )
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            flake: String::new(),
            output: d_output(),
            settings_file: None,
            profile: d_profile(),
            extra_portablectl_args: Vec::new(),
            input_overrides: BTreeMap::new(),
            health_check: HealthCheck::default(),
            keep_generations: d_keep(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        // A missing config file just means: no declarative services.
        match std::fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data)
                .map_err(Error::json(format!("cannot parse {}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::Io {
                context: format!("cannot read config {}", path.display()),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config() {
        let json = r#"{
          "eval_user": "flakelet",
          "services": {
            "grafana": { "flake": "github:me/grafana-svc" },
            "svc": {
              "flake": "git+https://example.com/svc",
              "settings_file": "/etc/flakelet/svc/settings.json",
              "profile": "trusted",
              "input_overrides": { "nixpkgs": "github:NixOS/nixpkgs/nixos-25.05" },
              "health_check": { "timeout": 5, "command": "curl -fs http://localhost:3000" },
              "keep_generations": 2
            }
          }
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        let grafana = &cfg.services["grafana"];
        assert!(grafana.output.starts_with("portableServices."));
        assert_eq!(grafana.keep_generations, 5);
        let svc = &cfg.services["svc"];
        assert_eq!(svc.profile, "trusted");
        assert_eq!(
            svc.health_check.command.as_deref(),
            Some("curl -fs http://localhost:3000")
        );
    }
}
