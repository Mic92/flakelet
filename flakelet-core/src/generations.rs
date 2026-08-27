use crate::error::{Error, Result};
use crate::state::write_json_atomic;
use crate::systemd::Units;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

/// Manifest stored in each generation directory
/// (<gcroot_dir>/<name>/gen-<N>/manifest.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub units: Units,
    pub flake_url: String,
    pub flake_rev: String,
    pub settings_hash: String,
    /// Store path of the driver expression that produced this generation.
    pub driver: PathBuf,
    /// Store path of the built service artifact.
    pub artifact: PathBuf,
    /// Exports of this generation (derivations already replaced by out paths).
    #[serde(default)]
    pub exports: serde_json::Value,
    #[serde(default)]
    pub state: Option<crate::svcstate::StateInfo>,
    pub created: u64,
}

pub struct Generations {
    dir: PathBuf, // <gcroot_dir>/<name>
}

impl Generations {
    pub fn new(gcroot_dir: &Path, service: &str) -> Self {
        Self {
            dir: gcroot_dir.join(service),
        }
    }

    fn io(&self) -> impl FnOnce(std::io::Error) -> Error {
        Error::io(format!("generation dir {}", self.dir.display()))
    }

    pub fn list(&self) -> Result<Vec<u32>> {
        let mut nums = Vec::new();
        match fs::read_dir(&self.dir) {
            Ok(entries) => {
                for entry in entries {
                    let name = entry.map_err(self.io())?.file_name();
                    if let Some(n) = name.to_str().and_then(|s| s.strip_prefix("gen-")) {
                        if let Ok(n) = n.parse() {
                            nums.push(n);
                        }
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(self.io()(e)),
        }
        nums.sort_unstable();
        Ok(nums)
    }

    pub fn manifest(&self, gen: u32) -> Result<Manifest> {
        let path = self.dir.join(format!("gen-{gen}")).join("manifest.json");
        let data = fs::read_to_string(&path)
            .map_err(Error::io(format!("cannot read {}", path.display())))?;
        serde_json::from_str(&data).map_err(Error::json(format!("corrupt {}", path.display())))
    }

    /// Create the next generation: manifest + gcroot symlinks for units and extra store paths.
    pub fn create(&self, manifest: &Manifest, extra_roots: &[String]) -> Result<u32> {
        let next = self.list()?.last().copied().unwrap_or(0) + 1;
        let dir = self.dir.join(format!("gen-{next}"));
        fs::create_dir_all(&dir).map_err(self.io())?;
        let roots = manifest
            .units
            .values()
            .map(|p| p.display().to_string())
            .chain([manifest.driver.display().to_string()])
            .chain(extra_roots.iter().cloned());
        for (i, target) in roots.enumerate() {
            symlink(&target, dir.join(format!("root-{i}"))).map_err(self.io())?;
        }
        write_json_atomic(&dir.join("manifest.json"), manifest)?;
        Ok(next)
    }

    /// Drop a generation that never became active.
    pub fn remove(&self, gen: u32) -> Result<()> {
        fs::remove_dir_all(self.dir.join(format!("gen-{gen}"))).map_err(self.io())
    }

    /// Remove generations older than the newest `keep`, but never the currently active one.
    pub fn prune(&self, keep: u32, current: Option<u32>) -> Result<Vec<u32>> {
        let gens = self.list()?;
        let cutoff = gens.len().saturating_sub(keep as usize);
        let mut removed = Vec::new();
        for &gen in &gens[..cutoff] {
            if Some(gen) == current {
                continue;
            }
            fs::remove_dir_all(self.dir.join(format!("gen-{gen}"))).map_err(self.io())?;
            removed.push(gen);
        }
        Ok(removed)
    }

    /// Remove all generations of this service.
    pub fn remove_all(&self) -> Result<()> {
        match fs::remove_dir_all(&self.dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.io()(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(unit_path: &str) -> Manifest {
        Manifest {
            version: 1,
            units: Units::from([("svc.service".into(), unit_path.into())]),
            flake_url: "github:me/svc".into(),
            flake_rev: "abc".into(),
            settings_hash: "h".into(),
            driver: "/nix/store/drv-driver.nix".into(),
            artifact: "/nix/store/artifact".into(),
            exports: serde_json::Value::Null,
            state: None,
            created: 0,
        }
    }

    #[test]
    fn create_list_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let gens = Generations::new(tmp.path(), "svc");
        assert!(gens.list().unwrap().is_empty());

        for i in 0..4 {
            let n = gens
                .create(
                    &manifest(&format!("/nix/store/unit{i}")),
                    &["/nix/store/dep".into()],
                )
                .unwrap();
            assert_eq!(n, i + 1);
        }
        assert_eq!(gens.list().unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(
            gens.manifest(2).unwrap().units["svc.service"],
            PathBuf::from("/nix/store/unit1")
        );
        // gc roots exist for unit, driver and extra path
        let dir = tmp.path().join("svc/gen-1");
        assert!(dir.join("root-0").is_symlink() && dir.join("root-2").is_symlink());

        // keep 2, but generation 1 is still active -> not removed
        assert_eq!(gens.prune(2, Some(1)).unwrap(), vec![2]);
        assert_eq!(gens.list().unwrap(), vec![1, 3, 4]);

        gens.remove_all().unwrap();
        assert!(gens.list().unwrap().is_empty());
    }
}
