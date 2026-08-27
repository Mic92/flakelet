use crate::config::SCHEMA_VERSION;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, ErrorKind};
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

/// Per-service state, stored at <state_dir>/<name>/state.json. Everything
/// about the active generation lives in its manifest, so a switch changes
/// one field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: u32,
    pub origin: Origin,
    /// Currently active generation number.
    pub generation: Option<u32>,
    /// Pinned flake URL set by `flakelet lock`.
    pub pin: Option<String>,
    /// Testing ref from `update --flake`. Cleared by the next regular update.
    pub override_flake: Option<String>,
    /// Set after a failed activation. The same artifact is not tried again.
    pub hold: Option<Hold>,
    /// Running an older cached generation because the last eval failed offline.
    pub degraded: bool,
    /// The entry must not run on this host until `enable`.
    pub disabled: Option<Disabled>,
    pub last_error: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            origin: Origin::default(),
            generation: None,
            pin: None,
            override_flake: None,
            hold: None,
            degraded: false,
            disabled: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DisabledBy {
    Operator,
    Export,
    Import,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Disabled {
    pub reason: String,
    pub by: DisabledBy,
    pub since: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hold {
    pub reason: String,
    pub artifact: PathBuf,
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

    pub fn held_for(&self, artifact: &Path) -> Option<&Hold> {
        self.hold.as_ref().filter(|h| h.artifact == artifact)
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
            hold: Some(Hold {
                reason: "health check failed".into(),
                artifact: "/nix/store/a".into(),
            }),
            ..State::default()
        };
        st.save(&path).unwrap();

        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded.origin, Origin::Manual);
        assert!(loaded.held_for(Path::new("/nix/store/a")).is_some());
        assert!(loaded.held_for(Path::new("/nix/store/b")).is_none());
    }
}
