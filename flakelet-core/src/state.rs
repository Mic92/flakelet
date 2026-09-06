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
    /// None until the first switch after this field was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<Change>,
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
            changed: None,
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

/// Why the active generation was switched to. Only `External` comes
/// from outside (`update --by-file`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum By {
    /// From a shell. `user` is SUDO_USER, DOAS_USER or USER.
    Manual {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
    },
    /// From a systemd unit without a terminal (boot, auto-update timer).
    Unit { unit: String },
    /// `flakelet rollback`.
    Rollback { from: u32 },
    External {
        /// e.g. "flakelet-relay"
        agent: String,
        /// The agent's own id for this run.
        id: String,
        /// Who asked the agent, for display.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
    },
}

impl Default for By {
    fn default() -> Self {
        By::Manual { user: None }
    }
}

impl By {
    /// `Unit` under systemd without a terminal, else `Manual`.
    pub fn detect() -> Self {
        use std::io::IsTerminal as _;
        if std::env::var_os("INVOCATION_ID").is_some() && !std::io::stderr().is_terminal() {
            if let Some(unit) = own_unit() {
                return By::Unit { unit };
            }
        }
        let user = ["SUDO_USER", "DOAS_USER", "USER"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
        By::Manual { user }
    }

    /// Read `--by-file`; anything but `External` is rejected.
    pub fn from_file(path: &Path) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "kebab-case")]
        enum Allowed {
            External {
                agent: String,
                id: String,
                #[serde(default)]
                caller: Option<String>,
            },
        }
        let ctx = || format!("--by-file {}", path.display());
        let data = fs::read(path).map_err(Error::io(ctx()))?;
        let Allowed::External { agent, id, caller } =
            serde_json::from_slice(&data).map_err(Error::json(ctx()))?;
        Ok(By::External { agent, id, caller })
    }
}

impl Change {
    pub fn now(generation: u32, by: By) -> Self {
        Self {
            generation,
            at: unix_time(),
            by,
        }
    }
}

pub fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn own_unit() -> Option<String> {
    let cg = fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cg.lines().find_map(|l| l.strip_prefix("0::"))?;
    path.rsplit('/')
        .find(|s| s.ends_with(".service"))
        .map(str::to_owned)
}

/// The last generation switch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Change {
    pub generation: u32,
    pub at: u64,
    pub by: By,
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
    fn by_file_accepts_only_external() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("by.json");
        fs::write(
            &f,
            r#"{"kind":"external","agent":"relay","id":"j1","caller":"me"}"#,
        )
        .unwrap();
        assert_eq!(
            By::from_file(&f).unwrap(),
            By::External {
                agent: "relay".into(),
                id: "j1".into(),
                caller: Some("me".into())
            }
        );
        fs::write(&f, r#"{"kind":"manual"}"#).unwrap();
        assert!(By::from_file(&f).is_err());
        fs::write(&f, r#"{"kind":"external","agent":"relay"}"#).unwrap();
        assert!(By::from_file(&f).is_err());
    }

    #[test]
    fn change_serializes_tagged() {
        let c = Change {
            generation: 3,
            at: 1,
            by: By::Rollback { from: 4 },
        };
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"generation":3,"at":1,"by":{"kind":"rollback","from":4}}"#
        );
        let st: State = serde_json::from_str(r#"{"version":1,"origin":"manual"}"#).unwrap();
        assert_eq!(st.changed, None);
    }

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
