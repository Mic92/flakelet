//! Persistent state a service declares through its units (`state.json` in
//! the artifact, derived by flakelet.lib). Drives export/import.

use crate::exports;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Folder {
    pub path: PathBuf,
    pub user: String,
    pub group: Option<String>,
    /// Owned by a DynamicUser=. systemd chowns it on start.
    pub dynamic: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StateInfo {
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub dump: Option<String>,
    #[serde(default)]
    pub restore: Option<String>,
}

impl StateInfo {
    pub fn paths(&self) -> Vec<&Path> {
        self.folders.iter().map(|f| f.path.as_path()).collect()
    }
}

/// Empty when exportable. `need_restore` checks the import direction.
pub fn blockers(
    state: Option<&StateInfo>,
    exports: &Value,
    providers_dir: &Path,
    need_restore: bool,
) -> Vec<String> {
    if state.is_none() {
        return vec!["generation was built without state.json, redeploy it".into()];
    }
    let mut out = Vec::new();
    let claims = exports::claims(exports);
    if !claims.is_empty() {
        let providers = exports::providers(providers_dir).unwrap_or_default();
        for claim in claims {
            match providers.iter().find(|p| p.claim() == claim) {
                None => out.push(format!("no provider for requires.{claim} on this host")),
                Some(p) if p.state.is_none() => out.push(format!(
                    "provider {} cannot {} requires.{claim}",
                    p.contract,
                    if need_restore { "restore" } else { "dump" }
                )),
                Some(_) => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn providers_without_state_block_export() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateInfo::default();
        let providers = dir.path().join("providers.d");
        fs::create_dir(&providers).unwrap();
        fs::write(
            providers.join("pg.json"),
            r#"{ "contract": "postgres/v1" }"#,
        )
        .unwrap();
        fs::write(
            providers.join("redis.json"),
            r#"{ "contract": "redis/v1", "state": { "dump": "/d", "restore": "/r" } }"#,
        )
        .unwrap();
        let exports = serde_json::json!({ "requires": { "postgres": {}, "redis": {}, "s3": {} } });

        let b = blockers(Some(&state), &exports, &providers, false);
        assert_eq!(b.len(), 2, "{b:?}");
        assert!(b[0].contains("postgres/v1 cannot dump"));
        assert!(b[1].contains("requires.s3"));
        assert_eq!(blockers(None, &exports, &providers, false).len(), 1);
    }
}
