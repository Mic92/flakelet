use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub origin: Origin,
    /// Currently attached generation number.
    pub generation: Option<u32>,
    /// Attached image files (*.raw paths).
    #[serde(default)]
    pub images: Vec<PathBuf>,
    /// Locked flake URL of the last successful update.
    pub locked_url: Option<String>,
    /// Pinned flake URL set by `flakelet lock`.
    pub pin: Option<String>,
    /// Set after a failed deploy; cleared when settings/rev change or --force.
    pub hold: Option<Hold>,
    /// Running an older cached generation because the last eval failed offline.
    #[serde(default)]
    pub degraded: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hold {
    pub reason: String,
    pub settings_hash: String,
    pub flake_rev: String,
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data)
                .map_err(Error::json(format!("corrupt {}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
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
    let dir = path
        .parent()
        .ok_or_else(|| Error::Deploy(format!("{} has no parent directory", path.display())))?;
    let context = || format!("cannot write {}", path.display());
    std::fs::create_dir_all(dir).map_err(Error::io(context()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let data = serde_json::to_vec_pretty(value).map_err(Error::json(context()))?;
    std::fs::write(&tmp, data).map_err(Error::io(context()))?;
    std::fs::rename(&tmp, path).map_err(Error::io(context()))
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
            hold: Some(Hold {
                reason: "health check failed".into(),
                settings_hash: "sha256-abc".into(),
                flake_rev: "deadbeef".into(),
            }),
            ..State::default()
        };
        st.save(&path).unwrap();

        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded.origin, Origin::Manual);
        assert_eq!(loaded.generation, Some(3));
        assert!(loaded.held_for("sha256-abc", "deadbeef"));
        assert!(!loaded.held_for("sha256-abc", "newrev"));
    }
}
