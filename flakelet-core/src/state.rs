use crate::config::SCHEMA_VERSION;
use crate::error::{Error, Result};
use crate::systemd::Units;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::Path;

/// Where a service definition came from. Declarative services are removed by
/// `reconcile` when they disappear from the host configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    #[default]
    Declarative,
    Manual,
}

/// Per-service state, stored at <state_dir>/<name>/state.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: u32,
    pub origin: Origin,
    /// Currently active generation number.
    pub generation: Option<u32>,
    /// Currently linked units: name -> unit file store path.
    pub units: Units,
    /// Exports of the active generation (also published under runtime_dir).
    pub exports: Value,
    /// Locked flake URL of the last successful update.
    pub locked_url: Option<String>,
    /// Pinned flake URL set by `flakelet lock`.
    pub pin: Option<String>,
    /// Set after a failed deploy; cleared when settings/rev change or --force.
    pub hold: Option<Hold>,
    /// Running an older cached generation because the last eval failed offline.
    pub degraded: bool,
    pub last_error: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            origin: Origin::default(),
            generation: None,
            units: Units::new(),
            exports: Value::Null,
            locked_url: None,
            pin: None,
            hold: None,
            degraded: false,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hold {
    pub reason: String,
    pub settings_hash: String,
    pub flake_rev: String,
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data)
                .map_err(Error::json(format!("corrupt {}", path.display()))),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::Io {
                context: format!("cannot read {}", path.display()),
                source,
            }),
        }
    }

    /// Atomic write (tmp file + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        write_json_atomic(path, self)
    }

    /// Whether an update with these inputs should be skipped due to a hold.
    pub fn held_for(&self, settings_hash: &str, flake_rev: &str) -> bool {
        self.hold
            .as_ref()
            .is_some_and(|h| h.settings_hash == settings_hash && h.flake_rev == flake_rev)
    }
}

/// Atomically write a JSON file (tmp file + rename in the same directory).
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let context = || format!("cannot write {}", path.display());
    let dir = path.parent().ok_or_else(|| Error::Io {
        context: context(),
        source: io::Error::new(ErrorKind::InvalidInput, "path has no parent directory"),
    })?;
    fs::create_dir_all(dir).map_err(Error::io(context()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let data = serde_json::to_vec_pretty(value).map_err(Error::json(context()))?;
    fs::write(&tmp, data).map_err(Error::io(context()))?;
    fs::rename(&tmp, path).map_err(Error::io(context()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_hold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc/state.json");
        assert!(State::load(&path).unwrap().generation.is_none());

        let st = State {
            origin: Origin::Manual,
            generation: Some(3),
            units: Units::from([("grafana.service".into(), "/nix/store/x".into())]),
            hold: Some(Hold {
                reason: "health check failed".into(),
                settings_hash: "abc".into(),
                flake_rev: "deadbeef".into(),
            }),
            ..State::default()
        };
        st.save(&path).unwrap();

        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded.origin, Origin::Manual);
        assert_eq!(loaded.units.len(), 1);
        assert!(loaded.held_for("abc", "deadbeef"));
        assert!(!loaded.held_for("abc", "newrev"));
    }
}
