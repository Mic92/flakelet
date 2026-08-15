use crate::config::{Config, ServiceConfig, SCHEMA_VERSION};
use crate::driver::DriverEntry;
use crate::error::{Error, Result};
use crate::generations::{Generations, Manifest};
use crate::state::{write_json_atomic, Hold, Origin, State};
use crate::systemd::Units;
use crate::{driver, exports, lock, nix, settings, systemd};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateOpts {
    pub force: bool,
    pub no_wait: bool,
    /// Tolerate network failures by keeping the current units (used at boot).
    pub offline_fallback: bool,
    /// Skip `--refresh` when resolving flake refs (offline use, tests).
    pub no_refresh: bool,
}

#[derive(Debug, Serialize, PartialEq)]
pub enum UpdateOutcome {
    UpToDate,
    Updated { generation: u32 },
    Degraded { reason: String },
    Held { reason: String },
    RolledBack { reason: String },
}

/// Options for the off-machine `check` pipeline.
#[derive(Debug, Clone, Default)]
pub struct CheckOpts {
    /// Also build the evaluated artifacts.
    pub build: bool,
    /// Bypass flake caches when resolving refs.
    pub refresh: bool,
    /// Root the evaluated derivations (and built artifacts) here, so a later
    /// build or deploy step still finds them after a garbage collection.
    pub gc_roots_dir: Option<PathBuf>,
    /// Directory for per-service out-links of built artifacts
    /// (default: gc_roots_dir).
    pub out_links: Option<PathBuf>,
}

/// Result of an off-machine `check`/`build` of one service.
#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub drv_path: String,
    /// Output path when the artifact was also built.
    pub out: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub flake: String,
    pub origin: Origin,
    pub generation: Option<u32>,
    pub units: Units,
    pub locked_url: Option<String>,
    pub pin: Option<String>,
    pub degraded: bool,
    pub held: Option<String>,
    pub last_error: Option<String>,
    pub updating: bool,
}

pub struct Manager {
    pub config: Config,
    system: String,
}

impl Manager {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            system: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        }
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
            let data = fs::read_to_string(&path)
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
        match fs::read_dir(&self.config.state_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(io())?;
                    if entry.path().is_dir() {
                        names.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
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
                    units: st.units,
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

    /// Resolve the flake refs of the given services (default: all with a
    /// flake ref) and render one driver expression covering them.
    /// Used by `check`/`build`/`driver`; needs no state, locks or root.
    pub fn render_driver(&self, names: &[String], refresh: bool) -> Result<String> {
        let services = self.services()?;
        for name in names {
            if !services.contains_key(name) {
                return Err(Error::UnknownService(name.clone()));
            }
        }
        let mut resolved = Vec::new();
        for (name, (svc, _)) in services {
            if svc.prebuilt.is_some() || (!names.is_empty() && !names.contains(&name)) {
                continue;
            }
            let nix = self.nix(&svc);
            let locked = nix.locked_url(&svc.flake, refresh)?;
            let nixpkgs = resolve_overrides(&name, &svc, &nix, refresh)?;
            let hash = hash_settings(&svc);
            resolved.push((name, svc, locked, nixpkgs, hash));
        }
        // Silently "checking" nothing would look like success in CI.
        if resolved.is_empty() {
            return Err(Error::NothingToCheck);
        }
        let entries: Vec<DriverEntry> = resolved
            .iter()
            .map(|(name, svc, locked, nixpkgs, hash)| DriverEntry {
                name,
                locked_url: &locked.url,
                locked_rev: &locked.rev,
                output: &svc.output,
                settings: &svc.settings,
                settings_hash: hash,
                nixpkgs_override: nixpkgs.as_ref().map(|n| n.url.as_str()),
            })
            .collect();
        Ok(driver::render(&self.config, &self.system, &entries))
    }

    /// Off-machine evaluation (and optionally build) of the configured
    /// services, e.g. in CI against a rendered config.json.
    pub fn check(&self, names: &[String], opts: &CheckOpts) -> Result<Vec<CheckResult>> {
        let expr = self.render_driver(names, opts.refresh)?;
        let nix = nix::Nix::new(&self.config, None);
        // O_EXCL temp file with an unpredictable name, written through the
        // open handle: nothing in a shared /tmp can plant a symlink or swap
        // the contents before `nix store add` reads it.
        let mut driver_file = tempfile::Builder::new()
            .prefix("flakelet-driver-")
            .suffix(".nix")
            .tempfile()
            .map_err(Error::io("create driver temp file"))?;
        std::io::Write::write_all(&mut driver_file, expr.as_bytes())
            .map_err(Error::io("write driver expression"))?;
        let driver_store = nix.add_driver(driver_file.path())?;
        let jobs = nix.eval_driver(&driver_store, opts.gc_roots_dir.as_deref())?;

        let mut results = Vec::new();
        for job in jobs {
            let drv_path = job
                .drv_path
                .ok_or_else(|| Error::NoDerivation(job.attr.clone()))?;
            let out = if opts.build {
                let out_link = opts
                    .out_links
                    .as_ref()
                    .or(opts.gc_roots_dir.as_ref())
                    .map(|dir| dir.join(&job.attr));
                Some(nix.build(&drv_path, out_link.as_deref())?)
            } else {
                None
            };
            results.push(CheckResult {
                name: job.attr,
                drv_path,
                out,
            });
        }
        Ok(results)
    }

    /// Closure diff between the running generation and a fresh evaluation.
    /// Read-only: takes no locks and writes no state, like `status`.
    pub fn diff(&self, name: &str, refresh: bool) -> Result<String> {
        let (svc, _) = self.service(name)?;
        let st = State::load(&self.state_path(name))?;
        let current = st
            .generation
            .ok_or_else(|| Error::NeverDeployed(name.into()))?;
        let manifest = Generations::new(&self.config.gcroot_dir, name).manifest(current)?;
        let old = manifest
            .artifact
            .ok_or_else(|| Error::NoArtifactRecorded(name.into()))?;
        let new = match &svc.prebuilt {
            Some(prebuilt) => prebuilt.clone(),
            None => {
                let opts = CheckOpts {
                    build: true,
                    refresh,
                    ..CheckOpts::default()
                };
                let result = self
                    .check(std::slice::from_ref(&name.to_string()), &opts)?
                    .pop()
                    .ok_or_else(|| Error::NoDerivation(name.into()))?;
                result
                    .out
                    .ok_or_else(|| Error::NoBuildOutput(name.into()))?
            }
        };
        nix::Nix::new(&self.config, svc.credentials.as_ref()).diff_closures(&old, &new)
    }

    /// Register (or redefine) a manually deployed service.
    pub fn deploy(
        &self,
        name: &str,
        svc: &ServiceConfig,
        opts: UpdateOpts,
    ) -> Result<UpdateOutcome> {
        if self.config.services.contains_key(name) {
            return Err(Error::DeclaredService(name.into()));
        }
        {
            let _locks = self.locks(name, !opts.no_wait, "deploy")?;
            write_json_atomic(&self.manual_config_path(name), svc)?;
        }
        self.update(name, opts)
    }

    /// Stop a service and delete its generations and state.
    pub fn remove(&self, name: &str) -> Result<()> {
        let _locks = self.locks(name, true, "remove")?;
        let st = State::load(&self.state_path(name))?;
        systemd::remove(&st.units)?;
        exports::unpublish(&self.config.runtime_dir, name)?;
        Generations::new(&self.config.gcroot_dir, name).remove_all()?;
        fs::remove_dir_all(self.service_dir(name))
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

    /// Re-link the units of all deployed services at boot, without evaluation.
    pub fn boot(&self) -> Result<Vec<String>> {
        let mut linked = Vec::new();
        for name in self.state_dirs()? {
            let st = State::load(&self.state_path(&name))?;
            if !st.units.is_empty() {
                systemd::relink(&st.units)?;
                exports::publish(&self.config.runtime_dir, &name, &st.exports)?;
                linked.push(name);
            }
        }
        Ok(linked)
    }

    pub fn lock_service(&self, name: &str) -> Result<String> {
        let (svc, _) = self.service(name)?;
        let _locks = self.locks(name, true, "lock")?;
        let mut st = State::load(&self.state_path(name))?;
        let locked = self.nix(&svc).locked_url(&svc.flake, true)?;
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
        self.service(name)?;
        let _locks = self.locks(name, true, "rollback")?;
        let mut st = State::load(&self.state_path(name))?;
        let gens = Generations::new(&self.config.gcroot_dir, name);
        let current = st
            .generation
            .ok_or_else(|| Error::NeverDeployed(name.into()))?;
        let target = *gens
            .list()?
            .iter()
            .rfind(|&&g| g < current)
            .ok_or_else(|| Error::NoOlderGeneration(name.into()))?;
        let manifest = gens.manifest(target)?;
        systemd::switch(&st.units, &manifest.units)?;
        exports::publish(&self.config.runtime_dir, name, &manifest.exports)?;
        st.generation = Some(target);
        st.units = manifest.units;
        st.exports = manifest.exports;
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

    fn nix(&self, svc: &ServiceConfig) -> nix::Nix {
        nix::Nix::new(&self.config, svc.credentials.as_ref())
    }

    fn try_update(
        &self,
        name: &str,
        svc: &ServiceConfig,
        st: &mut State,
        opts: UpdateOpts,
    ) -> Result<UpdateOutcome> {
        let nix = self.nix(svc);

        // Settings referencing store paths must exist; they are gc-rooted per generation.
        let soft_refs: Vec<String> = settings::store_paths(&svc.settings).into_iter().collect();
        for path in &soft_refs {
            if !Path::new(path).exists() {
                return Err(Error::DanglingStorePath {
                    service: name.into(),
                    path: path.clone(),
                });
            }
        }

        // Produce the service artifact: activate a prebuilt one as-is, or
        // resolve + evaluate + build the declared flake.
        let settings_hash = hash_settings(svc);
        let mut artifact = if let Some(prebuilt) = &svc.prebuilt {
            eprintln!("{name}: using prebuilt artifact {}", prebuilt.display());
            if !prebuilt.exists() {
                return Err(Error::DanglingStorePath {
                    service: name.into(),
                    path: prebuilt.display().to_string(),
                });
            }
            let meta = ArtifactMeta::load(prebuilt);
            Artifact {
                out: prebuilt.clone(),
                driver: prebuilt.clone(),
                flake_url: meta.flake_url,
                flake_rev: meta.flake_rev,
                settings_hash: meta.settings_hash,
                ..Artifact::default()
            }
        } else {
            // Flake ref: pinned URL wins.
            let flake_ref = st.pin.clone().unwrap_or_else(|| svc.flake.clone());
            eprintln!("{name}: resolving {flake_ref}");
            let locked = nix.locked_url(&flake_ref, !opts.no_refresh)?;

            if !opts.force && st.held_for(&settings_hash, &locked.rev) {
                return Ok(UpdateOutcome::Held {
                    reason: st
                        .hold
                        .as_ref()
                        .map(|h| h.reason.clone())
                        .unwrap_or_default(),
                });
            }
            let nixpkgs = resolve_overrides(name, svc, &nix, !opts.no_refresh)?;
            self.evaluate(name, svc, &nix, &locked, nixpkgs.as_ref(), &settings_hash)?
        };

        read_contents(name, &mut artifact)?;
        self.check_conflicts(name, &artifact)?;

        if artifact.units == st.units && !opts.force {
            // Exports may change without the units changing (e.g. a metrics hint).
            exports::publish(&self.config.runtime_dir, name, &artifact.exports)?;
            st.degraded = false;
            st.last_error = None;
            return Ok(UpdateOutcome::UpToDate);
        }
        self.activate(name, svc, st, artifact, soft_refs)
    }

    /// Render, store, evaluate and build the driver expression for one service.
    fn evaluate(
        &self,
        name: &str,
        svc: &ServiceConfig,
        nix: &nix::Nix,
        locked: &nix::LockedFlake,
        nixpkgs_override: Option<&nix::LockedFlake>,
        settings_hash: &str,
    ) -> Result<Artifact> {
        let expr = driver::render(
            &self.config,
            &self.system,
            &[DriverEntry {
                name,
                locked_url: &locked.url,
                locked_rev: &locked.rev,
                output: &svc.output,
                settings: &svc.settings,
                settings_hash,
                nixpkgs_override: nixpkgs_override.map(|n| n.url.as_str()),
            }],
        );
        let driver_file = self.service_dir(name).join("driver.nix");
        fs::create_dir_all(self.service_dir(name)).map_err(Error::io("create service dir"))?;
        fs::write(&driver_file, &expr).map_err(Error::io("write driver.nix"))?;
        let driver_store = nix.add_driver(&driver_file)?;

        eprintln!("{name}: evaluating {}", driver_store.display());
        let jobs = nix.eval_driver(&driver_store, None)?;
        let job = jobs
            .iter()
            .find(|j| j.attr == name)
            .and_then(|j| j.drv_path.as_deref())
            .ok_or_else(|| Error::NoDerivation(name.into()))?;
        eprintln!("{name}: building {job}");
        let out = nix.build(job, Some(&self.service_dir(name).join("result")))?;
        Ok(Artifact {
            out,
            driver: driver_store,
            flake_url: locked.url.clone(),
            flake_rev: locked.rev.clone(),
            settings_hash: settings_hash.into(),
            flake_roots: nix.flake_source_paths(&locked.url)?,
            ..Artifact::default()
        })
    }

    /// Commit the artifact as a new generation and switch the units over,
    /// rolling back to the previous generation if activation fails.
    fn activate(
        &self,
        name: &str,
        svc: &ServiceConfig,
        st: &mut State,
        artifact: Artifact,
        soft_refs: Vec<String>,
    ) -> Result<UpdateOutcome> {
        let units = artifact.units.clone();
        let health_check = artifact.health_check.clone();
        // Commit the generation (gc roots) before touching any unit. Root the
        // flake source + inputs too, so re-evals work offline.
        let gens = Generations::new(&self.config.gcroot_dir, name);
        let mut extra_roots = soft_refs;
        extra_roots.push(artifact.out.display().to_string());
        extra_roots.extend(artifact.flake_roots);
        let manifest = Manifest {
            version: SCHEMA_VERSION,
            units: units.clone(),
            flake_url: artifact.flake_url.clone(),
            flake_rev: artifact.flake_rev.clone(),
            settings_hash: artifact.settings_hash.clone(),
            driver: artifact.driver,
            artifact: Some(artifact.out.clone()),
            health_check: health_check.clone(),
            exports: artifact.exports.clone(),
            created: unix_time(),
        };
        let generation = gens.create(&manifest, &extra_roots)?;

        eprintln!("{name}: activating generation {generation}");
        let previous_units = st.units.clone();
        let result = systemd::switch(&previous_units, &units)
            .and_then(|()| health_check_run(name, &units, health_check.as_deref()));

        if let Err(err) = result {
            let reason = err.to_string();
            // Restore the previous units; their settings are baked into their store paths.
            if !previous_units.is_empty() {
                systemd::switch(&units, &previous_units).map_err(|e| Error::RollbackFailed {
                    service: name.into(),
                    source: Box::new(e),
                })?;
            }
            st.hold = Some(Hold {
                reason: reason.clone(),
                settings_hash: artifact.settings_hash,
                flake_rev: artifact.flake_rev,
            });
            st.last_error = Some(reason.clone());
            return Ok(UpdateOutcome::RolledBack { reason });
        }

        exports::publish(&self.config.runtime_dir, name, &artifact.exports)?;
        st.generation = Some(generation);
        st.units = units;
        st.exports = artifact.exports;
        st.locked_url = Some(artifact.flake_url);
        st.hold = None;
        st.degraded = false;
        st.last_error = None;
        gens.prune(svc.keep_generations, Some(generation))?;
        Ok(UpdateOutcome::Updated { generation })
    }

    /// Refuse unit names or port claims that already belong to another
    /// flakelet-managed service.
    fn check_conflicts(&self, name: &str, artifact: &Artifact) -> Result<()> {
        for other in self.state_dirs()? {
            if other == name {
                continue;
            }
            let st = State::load(&self.state_path(&other))?;
            if let Some(unit) = artifact.units.keys().find(|u| st.units.contains_key(*u)) {
                return Err(Error::UnitConflict {
                    service: name.into(),
                    unit: unit.clone(),
                    owner: other,
                });
            }
            exports::check_port_conflicts(name, &artifact.exports, &other, &st.exports)?;
        }
        Ok(())
    }
}

/// A produced service artifact plus its provenance and contents.
#[derive(Default)]
struct Artifact {
    out: PathBuf,
    driver: PathBuf,
    flake_url: String,
    flake_rev: String,
    settings_hash: String,
    flake_roots: Vec<String>,
    units: Units,
    health_check: Option<PathBuf>,
    exports: serde_json::Value,
}

/// meta.json inside a service artifact; missing fields stay empty.
#[derive(Default, serde::Deserialize)]
struct ArtifactMeta {
    #[serde(default)]
    flake_url: String,
    #[serde(default)]
    flake_rev: String,
    #[serde(default)]
    settings_hash: String,
}

impl ArtifactMeta {
    fn load(artifact: &Path) -> Self {
        fs::read_to_string(artifact.join("meta.json"))
            .ok()
            .and_then(|data| serde_json::from_str(&data).ok())
            .unwrap_or_default()
    }
}

/// Read units/, the optional health-check and exports from a built driver output.
fn read_contents(name: &str, artifact: &mut Artifact) -> Result<()> {
    let units_dir = artifact.out.join("units");
    let context = || format!("read {}", units_dir.display());
    for entry in fs::read_dir(&units_dir).map_err(Error::io(context()))? {
        let entry = entry.map_err(Error::io(context()))?;
        let target = fs::canonicalize(entry.path()).map_err(Error::io(context()))?;
        artifact
            .units
            .insert(entry.file_name().to_string_lossy().into_owned(), target);
    }
    if artifact.units.is_empty() {
        return Err(Error::NoUnits(name.into()));
    }
    artifact.health_check = Some(artifact.out.join("health-check")).filter(|p| p.exists());
    artifact.exports = match fs::read_to_string(artifact.out.join("exports.json")) {
        Ok(data) => serde_json::from_str(&data)
            .map_err(Error::json(format!("corrupt exports.json of {name}")))?,
        Err(e) if e.kind() == ErrorKind::NotFound => serde_json::Value::Null,
        Err(e) => return Err(Error::io(format!("read exports.json of {name}"))(e)),
    };
    Ok(())
}

fn health_check_run(name: &str, units: &Units, script: Option<&Path>) -> Result<()> {
    if let Some(unit) = systemd::any_failed(units)? {
        return Err(Error::UnitFailed {
            service: name.into(),
            unit,
        });
    }
    let Some(script) = script else { return Ok(()) };
    let status = Command::new(script)
        .status()
        .map_err(|source| Error::Spawn {
            program: script.display().to_string(),
            source,
        })?;
    if !status.success() {
        return Err(Error::HealthCheckFailed {
            service: name.into(),
            script: script.into(),
        });
    }
    Ok(())
}

/// Resolve input_overrides to locked references. Only 'nixpkgs' is supported:
/// pkgs is the one dependency flakelet itself injects, so it can be swapped
/// out here, while other inputs would require rewriting the flake's own lock,
/// which builtins.getFlake cannot do purely (and the service contract forbids
/// flake inputs anyway).
fn resolve_overrides(
    name: &str,
    svc: &ServiceConfig,
    nix: &nix::Nix,
    refresh: bool,
) -> Result<Option<nix::LockedFlake>> {
    let mut nixpkgs = None;
    for (input, flake_ref) in &svc.input_overrides {
        if input != "nixpkgs" {
            return Err(Error::UnsupportedInputOverride {
                service: name.into(),
                input: input.clone(),
            });
        }
        nixpkgs = Some(nix.locked_url(flake_ref, refresh)?);
    }
    Ok(nixpkgs)
}

/// Change-detection hash over the parts of the definition that affect the build.
fn hash_settings(svc: &ServiceConfig) -> String {
    let mut hasher = DefaultHasher::new();
    svc.output.hash(&mut hasher);
    svc.settings.to_string().hash(&mut hasher);
    svc.input_overrides.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(dir: &Path) -> Manager {
        Manager::new(Config {
            state_dir: dir.join("state"),
            gcroot_dir: dir.join("gcroots"),
            cache_dir: dir.join("cache"),
            runtime_dir: dir.join("run"),
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
        // Leftover state of a service that used to be declarative but has no units.
        State::default().save(&mgr.state_path("gone")).unwrap();

        let services = mgr.services().unwrap();
        assert_eq!(services.len(), 2);
        assert_eq!(services["decl"].1, Origin::Declarative);
        assert_eq!(services["man"].1, Origin::Manual);

        assert_eq!(mgr.reconcile().unwrap(), vec!["gone".to_string()]);
        assert!(mgr.state_path("man").exists());
        assert!(!mgr.service_dir("gone").exists());
    }

    #[test]
    fn foreign_unit_names_and_ports_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        State {
            units: Units::from([("web.service".into(), "/nix/store/x".into())]),
            exports: serde_json::json!({ "ports": { "http": { "port": 80 } } }),
            ..Default::default()
        }
        .save(&mgr.state_path("other"))
        .unwrap();

        let mut artifact = Artifact {
            units: Units::from([("web.service".into(), "/nix/store/y".into())]),
            ..Artifact::default()
        };
        assert!(matches!(
            mgr.check_conflicts("mine", &artifact),
            Err(Error::UnitConflict { .. })
        ));
        assert!(mgr.check_conflicts("other", &artifact).is_ok());

        artifact.units = Units::from([("api.service".into(), "/nix/store/y".into())]);
        artifact.exports = serde_json::json!({ "ports": { "http": { "port": 80 } } });
        assert!(matches!(
            mgr.check_conflicts("mine", &artifact),
            Err(Error::PortConflict { port: 80, .. })
        ));
    }
}
