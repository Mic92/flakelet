use crate::config::{Config, ServiceConfig};
use crate::error::{Error, Result};
use crate::generations::{Generations, Manifest};
use crate::state::{write_json_atomic, Hold, Origin, State};
use crate::{lock, nix, portablectl, settings};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateOpts {
    pub force: bool,
    pub no_wait: bool,
    /// Tolerate network failures by keeping the current attachment (used at boot).
    pub offline_fallback: bool,
}

#[derive(Debug, Serialize, PartialEq)]
pub enum UpdateOutcome {
    UpToDate,
    Updated { generation: u32 },
    Degraded { reason: String },
    Held { reason: String },
    RolledBack { reason: String },
}

#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub flake: String,
    pub origin: Origin,
    pub generation: Option<u32>,
    pub images: Vec<PathBuf>,
    pub locked_url: Option<String>,
    pub pin: Option<String>,
    pub degraded: bool,
    pub held: Option<String>,
    pub last_error: Option<String>,
    pub updating: bool,
}

pub struct Manager {
    pub config: Config,
}

struct PreparedSettings {
    store_path: String,
    hash: String,
    soft_refs: Vec<String>,
    nar_hashes: BTreeMap<String, String>,
}

impl Manager {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub fn load(config_path: &Path) -> Result<Self> {
        Ok(Self::new(Config::load(config_path)?))
    }

    fn service_dir(&self, name: &str) -> PathBuf {
        self.config.state_dir.join(name)
    }
    fn state_path(&self, name: &str) -> PathBuf {
        self.service_dir(name).join("state.json")
    }
    fn manual_config_path(&self, name: &str) -> PathBuf {
        self.service_dir(name).join("service.json")
    }
    fn service_lock(&self, name: &str) -> PathBuf {
        self.service_dir(name).join("lock")
    }
    fn global_lock(&self) -> PathBuf {
        self.config.state_dir.join("lock")
    }

    fn locks(&self, name: &str, wait: bool, info: &str) -> Result<(lock::Lock, lock::Lock)> {
        let global = lock::acquire(&self.global_lock(), false, wait, info)?;
        let service = lock::acquire(&self.service_lock(name), true, wait, info)?;
        Ok((global, service))
    }

    /// Declarative services from config.json plus manually deployed ones.
    /// On name collision the declarative definition wins.
    pub fn services(&self) -> Result<BTreeMap<String, (ServiceConfig, Origin)>> {
        let mut all: BTreeMap<String, (ServiceConfig, Origin)> = self
            .config
            .services
            .iter()
            .map(|(n, s)| (n.clone(), (s.clone(), Origin::Declarative)))
            .collect();
        for name in self.state_dirs()? {
            let path = self.manual_config_path(&name);
            if all.contains_key(&name) || !path.exists() {
                continue;
            }
            let data = std::fs::read_to_string(&path)
                .map_err(Error::io(format!("cannot read {}", path.display())))?;
            let svc = serde_json::from_str(&data)
                .map_err(Error::json(format!("corrupt {}", path.display())))?;
            all.insert(name, (svc, Origin::Manual));
        }
        Ok(all)
    }

    fn service(&self, name: &str) -> Result<(ServiceConfig, Origin)> {
        self.services()?
            .remove(name)
            .ok_or_else(|| Error::UnknownService(name.into()))
    }

    fn state_dirs(&self) -> Result<Vec<String>> {
        let io = || Error::io(format!("read {}", self.config.state_dir.display()));
        let mut names = Vec::new();
        match std::fs::read_dir(&self.config.state_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(io())?;
                    if entry.path().is_dir() {
                        names.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io()(e)),
        }
        names.sort();
        Ok(names)
    }

    pub fn status(&self) -> Result<Vec<ServiceStatus>> {
        self.services()?
            .into_iter()
            .map(|(name, (svc, origin))| {
                let st = State::load(&self.state_path(&name))?;
                let updating =
                    lock::acquire(&self.service_lock(&name), true, false, "status probe").is_err();
                Ok(ServiceStatus {
                    name,
                    flake: svc.flake,
                    origin,
                    generation: st.generation,
                    images: st.images,
                    locked_url: st.locked_url,
                    pin: st.pin,
                    degraded: st.degraded,
                    held: st.hold.map(|h| h.reason),
                    last_error: st.last_error,
                    updating,
                })
            })
            .collect()
    }

    /// Register (or redefine) a manually deployed service.
    pub fn deploy(
        &self,
        name: &str,
        svc: &ServiceConfig,
        opts: UpdateOpts,
    ) -> Result<UpdateOutcome> {
        if self.config.services.contains_key(name) {
            return Err(Error::Deploy(format!(
                "'{name}' is declared in the host configuration; not overriding it manually"
            )));
        }
        {
            let _locks = self.locks(name, !opts.no_wait, "deploy")?;
            write_json_atomic(&self.manual_config_path(name), svc)?;
        }
        self.update(name, opts)
    }

    /// Detach a service and delete its generations and state.
    pub fn remove(&self, name: &str) -> Result<()> {
        let _locks = self.locks(name, true, "remove")?;
        let st = State::load(&self.state_path(name))?;
        for image in &st.images {
            portablectl::detach(image)?;
        }
        Generations::new(&self.config.gcroot_dir, name).remove_all()?;
        std::fs::remove_dir_all(self.service_dir(name))
            .map_err(Error::io(format!("remove state of {name}")))?;
        Ok(())
    }

    /// Remove declarative services that are no longer present in the host configuration.
    pub fn reconcile(&self) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        for name in self.state_dirs()? {
            if self.config.services.contains_key(&name) || self.manual_config_path(&name).exists() {
                continue;
            }
            let st = State::load(&self.state_path(&name))?;
            if st.origin == Origin::Declarative {
                self.remove(&name)?;
                removed.push(name);
            }
        }
        Ok(removed)
    }

    pub fn lock_service(&self, name: &str) -> Result<String> {
        let (svc, _) = self.service(name)?;
        let _locks = self.locks(name, true, "lock")?;
        let mut st = State::load(&self.state_path(name))?;
        let locked = nix::Nix::new(&self.config).locked_url(&svc.flake)?;
        st.pin = Some(locked.url.clone());
        st.save(&self.state_path(name))?;
        Ok(locked.url)
    }

    pub fn unlock_service(&self, name: &str) -> Result<()> {
        self.service(name)?;
        let _locks = self.locks(name, true, "unlock")?;
        let mut st = State::load(&self.state_path(name))?;
        st.pin = None;
        st.save(&self.state_path(name))
    }

    pub fn gc(&self, keep_override: Option<u32>) -> Result<()> {
        let _global = lock::acquire(&self.global_lock(), true, true, "gc")?;
        for (name, (svc, _)) in self.services()? {
            let st = State::load(&self.state_path(&name))?;
            let keep = keep_override.unwrap_or(svc.keep_generations);
            Generations::new(&self.config.gcroot_dir, &name).prune(keep, st.generation)?;
        }
        Ok(())
    }

    pub fn rollback(&self, name: &str) -> Result<u32> {
        let (svc, _) = self.service(name)?;
        let _locks = self.locks(name, true, "rollback")?;
        let mut st = State::load(&self.state_path(name))?;
        let gens = Generations::new(&self.config.gcroot_dir, name);
        let current = st
            .generation
            .ok_or_else(|| Error::Deploy(format!("'{name}' was never deployed")))?;
        let target = *gens
            .list()?
            .iter()
            .rfind(|&&g| g < current)
            .ok_or_else(|| Error::Deploy("no older generation to roll back to".into()))?;
        let manifest = gens.manifest(target)?;
        self.switch_attachment(&svc, &st.images, &manifest.images)?;
        st.generation = Some(target);
        st.images = manifest.images;
        st.save(&self.state_path(name))?;
        Ok(target)
    }

    pub fn update(&self, name: &str, opts: UpdateOpts) -> Result<UpdateOutcome> {
        let (svc, origin) = self.service(name)?;
        let _locks = self.locks(name, !opts.no_wait, "update")?;
        let state_path = self.state_path(name);
        let mut st = State::load(&state_path)?;
        st.origin = origin;

        match self.try_update(name, &svc, &mut st, opts) {
            Ok(outcome) => {
                st.save(&state_path)?;
                Ok(outcome)
            }
            Err(err) => {
                let msg = err.to_string();
                st.last_error = Some(msg.clone());
                if opts.offline_fallback && err.is_network_error() {
                    st.degraded = true;
                    st.save(&state_path)?;
                    return Ok(UpdateOutcome::Degraded { reason: msg });
                }
                st.save(&state_path)?;
                Err(err)
            }
        }
    }

    fn try_update(
        &self,
        name: &str,
        svc: &ServiceConfig,
        st: &mut State,
        opts: UpdateOpts,
    ) -> Result<UpdateOutcome> {
        let nix = nix::Nix::new(&self.config);

        let prepared = self.prepare_settings(&nix, name, svc)?;

        // Flake ref: pinned URL wins.
        let flake_ref = st.pin.clone().unwrap_or_else(|| svc.flake.clone());
        let locked = nix.locked_url(&flake_ref)?;

        if !opts.force && st.held_for(&prepared.hash, &locked.rev) {
            return Ok(UpdateOutcome::Held {
                reason: st
                    .hold
                    .as_ref()
                    .map(|h| h.reason.clone())
                    .unwrap_or_default(),
            });
        }

        // Evaluate and build all images.
        let expr = nix::select_expr(&prepared.store_path, &prepared.hash, &prepared.nar_hashes);
        let jobs = nix.eval(svc, &locked.url, &expr)?;
        let build_root = self.service_dir(name).join("build");
        std::fs::create_dir_all(&build_root).map_err(Error::io("create build dir"))?;
        let mut images = Vec::new();
        for (i, job) in jobs.iter().enumerate() {
            let drv = job
                .drv_path
                .as_deref()
                .ok_or_else(|| Error::Deploy(format!("missing drvPath for {}", job.attr)))?;
            let out = nix.build(drv, &build_root.join(format!("out-{i}")))?;
            images.extend(raw_images(&out)?);
        }
        if images.is_empty() {
            return Err(Error::Deploy("built outputs contain no *.raw image".into()));
        }
        images.sort();

        if images == st.images && !opts.force {
            st.degraded = false;
            st.last_error = None;
            return Ok(UpdateOutcome::UpToDate);
        }

        // Commit generation (gc roots) before touching the attachment. Root the
        // flake source + inputs too, so re-evals work offline.
        let gens = Generations::new(&self.config.gcroot_dir, name);
        let mut extra_roots = prepared.soft_refs;
        extra_roots.push(prepared.store_path);
        extra_roots.extend(nix.flake_source_paths(&locked.url)?);
        let manifest = Manifest {
            images: images.clone(),
            flake_url: locked.url.clone(),
            flake_rev: locked.rev.clone(),
            settings_hash: prepared.hash.clone(),
            created: unix_time(),
        };
        let generation = gens.create(&manifest, &extra_roots)?;

        let previous_images = st.images.clone();
        let result = self
            .switch_attachment(svc, &previous_images, &images)
            .and_then(|()| self.health_check(svc, &images));

        if let Err(err) = result {
            let reason = err.to_string();
            // Restore the previous attachment; its settings are baked into those images.
            if !previous_images.is_empty() {
                self.switch_attachment(svc, &images, &previous_images)
                    .map_err(|e| {
                        Error::Deploy(format!("rollback after failed deploy also failed: {e}"))
                    })?;
            }
            st.hold = Some(Hold {
                reason: reason.clone(),
                settings_hash: prepared.hash,
                flake_rev: locked.rev,
            });
            st.last_error = Some(reason.clone());
            return Ok(UpdateOutcome::RolledBack { reason });
        }

        st.generation = Some(generation);
        st.images = images;
        st.locked_url = Some(locked.url);
        st.hold = None;
        st.degraded = false;
        st.last_error = None;
        gens.prune(svc.keep_generations, Some(generation))?;
        Ok(UpdateOutcome::Updated { generation })
    }

    fn prepare_settings(
        &self,
        nix: &nix::Nix,
        name: &str,
        svc: &ServiceConfig,
    ) -> Result<PreparedSettings> {
        let path = match &svc.settings_file {
            Some(p) => p.clone(),
            None => {
                // No settings configured: use an empty object.
                let empty = self.service_dir(name).join("empty-settings.json");
                write_json_atomic(&empty, &serde_json::json!({}))?;
                empty
            }
        };
        let resolved = std::fs::canonicalize(&path)
            .map_err(Error::io(format!("settings file {}", path.display())))?;
        let store_path = if resolved.starts_with("/nix/store") {
            resolved.display().to_string()
        } else {
            nix.add_to_store(&resolved)?
        };
        let hash = nix.sha256_of_file(&resolved)?;

        let data = std::fs::read_to_string(&resolved)
            .map_err(Error::io(format!("read {}", resolved.display())))?;
        let value: serde_json::Value = serde_json::from_str(&data).map_err(Error::json(
            format!("settings {} is not valid JSON", resolved.display()),
        ))?;
        let mut nar_hashes = BTreeMap::new();
        let mut soft_refs = Vec::new();
        for p in settings::store_paths(&value) {
            if !Path::new(&p).exists() {
                return Err(Error::DanglingStorePath(p));
            }
            nar_hashes.insert(p.clone(), nix.nar_hash(&p)?);
            soft_refs.push(p);
        }
        Ok(PreparedSettings {
            store_path,
            hash,
            soft_refs,
            nar_hashes,
        })
    }

    /// Attach `new` images, reattaching where an image of the same name is already
    /// attached, and detach images from `old` that are no longer present.
    fn switch_attachment(
        &self,
        svc: &ServiceConfig,
        old: &[PathBuf],
        new: &[PathBuf],
    ) -> Result<()> {
        let attached = portablectl::list()?;
        for image in new {
            let reattach = attached.contains(&portablectl::image_name(image));
            portablectl::attach(image, &svc.profile, &svc.extra_portablectl_args, reattach)?;
        }
        let new_names: Vec<String> = new.iter().map(|p| portablectl::image_name(p)).collect();
        for image in old {
            if !new_names.contains(&portablectl::image_name(image)) {
                portablectl::detach(image)?;
            }
        }
        Ok(())
    }

    fn health_check(&self, svc: &ServiceConfig, images: &[PathBuf]) -> Result<()> {
        std::thread::sleep(std::time::Duration::from_secs(svc.health_check.timeout));
        for image in images {
            let name = portablectl::image_name(image);
            let failed = Command::new("systemctl")
                .args(["is-failed", "--quiet", &format!("{name}*")])
                .status()
                .map_err(|source| Error::Spawn {
                    program: "systemctl".into(),
                    source,
                })?;
            // `systemctl is-failed --quiet` exits 0 if any matching unit failed.
            if failed.success() {
                return Err(Error::Deploy(format!(
                    "units of image '{name}' failed after attach"
                )));
            }
        }
        if let Some(cmd) = &svc.health_check.command {
            let status = Command::new("/bin/sh")
                .arg("-c")
                .arg(cmd)
                .status()
                .map_err(|source| Error::Spawn {
                    program: cmd.clone(),
                    source,
                })?;
            if !status.success() {
                return Err(Error::Deploy(format!("health check command failed: {cmd}")));
            }
        }
        Ok(())
    }
}

/// All *.raw files inside a portableService output directory (or the path itself).
fn raw_images(out: &Path) -> Result<Vec<PathBuf>> {
    if out.extension().is_some_and(|e| e == "raw") {
        return Ok(vec![out.to_path_buf()]);
    }
    let context = || format!("read {}", out.display());
    let mut images = Vec::new();
    for entry in std::fs::read_dir(out).map_err(Error::io(context()))? {
        let path = entry.map_err(Error::io(context()))?.path();
        if path.extension().is_some_and(|e| e == "raw") {
            images.push(path);
        }
    }
    Ok(images)
}

fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn manager(dir: &Path) -> Manager {
        Manager::new(Config {
            state_dir: dir.join("state"),
            gcroot_dir: dir.join("gcroots"),
            cache_dir: dir.join("cache"),
            ..Config::default()
        })
    }

    #[test]
    fn manual_services_are_merged_and_survive_reconcile() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = manager(tmp.path());
        mgr.config.services.insert(
            "decl".into(),
            ServiceConfig {
                flake: "github:me/decl".into(),
                ..Default::default()
            },
        );

        // Register a manual service definition + state.
        let manual = ServiceConfig {
            flake: "github:me/manual".into(),
            ..Default::default()
        };
        write_json_atomic(&mgr.manual_config_path("man"), &manual).unwrap();
        State {
            origin: Origin::Manual,
            ..Default::default()
        }
        .save(&mgr.state_path("man"))
        .unwrap();
        // Leftover state of a service that used to be declarative but has no images.
        State {
            origin: Origin::Declarative,
            ..Default::default()
        }
        .save(&mgr.state_path("gone"))
        .unwrap();

        let services = mgr.services().unwrap();
        assert_eq!(services.len(), 2);
        assert_eq!(services["decl"].1, Origin::Declarative);
        assert_eq!(services["man"].1, Origin::Manual);

        assert_eq!(mgr.reconcile().unwrap(), vec!["gone".to_string()]);
        assert!(mgr.state_path("man").exists());
        assert!(!mgr.service_dir("gone").exists());
    }
}
