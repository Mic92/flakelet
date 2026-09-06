use crate::config::{Config, ServiceConfig, SCHEMA_VERSION};
use crate::driver::DriverEntry;
use crate::error::{Error, Result};
use crate::generations::{Generations, Manifest};
use crate::state::{
    unix_time, write_json_atomic, By, Change, Disabled, DisabledBy, Hold, Origin, State,
};
use crate::svcstate::StateInfo;
use crate::systemd::Units;
use crate::transfer::{self, ExportMeta};
use crate::{driver, exports, lock, nix, settings, svcstate, systemd};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::slice;

#[derive(Debug, Clone, Default)]
pub struct UpdateOpts {
    /// Stored as `changed.by` if this creates a generation.
    pub by: By,
    pub force: bool,
    pub no_wait: bool,
    /// Tolerate network failures by keeping the current units (used at boot).
    pub offline_fallback: bool,
    /// Skip `--refresh` when resolving flake refs (offline use, tests).
    pub no_refresh: bool,
    /// Testing override for the flake ref. The next update without it
    /// reverts to the configured ref.
    pub flake: Option<String>,
    /// Unpacked export archive to restore state from before activation.
    pub restore_from: Option<PathBuf>,
    /// Import: clear state folders instead of requiring them empty.
    pub replace: bool,
}

/// Precedence: --flake override, then pin, then the configured ref.
fn effective_flake_ref<'a>(
    override_ref: Option<&'a str>,
    pin: Option<&'a str>,
    configured: &'a str,
) -> &'a str {
    override_ref.or(pin).unwrap_or(configured)
}

#[derive(Debug, Serialize, PartialEq)]
pub enum UpdateOutcome {
    UpToDate,
    Updated { generation: u32 },
    Degraded { reason: String },
    Held { reason: String },
    RolledBack { reason: String },
    Disabled { reason: String },
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
    /// Contract claims no provider on this host announces.
    pub missing_providers: Vec<String>,
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
    /// Testing ref when the active generation came from `update --flake`.
    pub override_flake: Option<String>,
    pub degraded: bool,
    pub held: Option<String>,
    pub disabled: Option<Disabled>,
    pub last_error: Option<String>,
    pub updating: bool,
    pub failed_units: Vec<String>,
    pub unit_states: Vec<systemd::UnitState>,
    pub missing_providers: Vec<String>,
    pub state: Option<StateInfo>,
    pub export_blockers: Vec<String>,
    /// Last generation switch, from state.json.
    pub changed: Option<Change>,
}

impl ServiceStatus {
    fn named(name: String) -> Self {
        Self {
            name,
            flake: String::new(),
            origin: Origin::Manual,
            generation: None,
            units: Units::new(),
            locked_url: None,
            pin: None,
            override_flake: None,
            degraded: false,
            held: None,
            disabled: None,
            last_error: None,
            updating: false,
            failed_units: Vec::new(),
            unit_states: Vec::new(),
            missing_providers: Vec::new(),
            state: None,
            export_blockers: Vec::new(),
            changed: None,
        }
    }
}

pub type Failures = Vec<(String, Error)>;

pub struct Manager {
    pub config: Config,
    /// Whether <state_dir> belongs to this config. False when checking
    /// another machine's rendered config.
    local: bool,
}

impl Manager {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            local: true,
        }
    }

    /// For `check`/`build`/`driver` against a rendered config of another machine.
    pub fn off_machine(config: Config) -> Self {
        Self {
            config,
            local: false,
        }
    }

    pub fn load(config_path: &Path) -> Result<Self> {
        Ok(Self::new(Config::load(config_path)?))
    }

    /// The active generation's manifest. Units, exports, state folders and
    /// flake URL are only ever read from here.
    pub fn current(&self, name: &str, st: &State) -> Result<Option<Manifest>> {
        st.generation
            .map(|g| Generations::new(&self.config.gcroot_dir, name).manifest(g))
            .transpose()
    }

    fn running(&self, name: &str, st: &State) -> Result<Manifest> {
        if let Some(d) = &st.disabled {
            return Err(Error::Disabled {
                service: name.into(),
                reason: d.reason.clone(),
            });
        }
        self.current(name, st)?
            .ok_or_else(|| Error::NeverDeployed(name.into()))
    }

    fn set_disabled(&self, name: &str, st: &mut State, reason: &str, by: DisabledBy) -> Result<()> {
        if let Some(m) = self.current(name, st)? {
            systemd::remove(&m.units)?;
        }
        exports::unpublish(&self.config.runtime_dir, name)?;
        st.disabled = Some(Disabled {
            reason: reason.into(),
            by,
            since: unix_time(),
        });
        st.save(&self.state_path(name))
    }

    pub fn disable(&self, name: &str, reason: &str) -> Result<()> {
        self.service(name)?;
        let _locks = self.locks(name, true, "disable")?;
        let mut st = State::load(&self.state_path(name))?;
        self.set_disabled(name, &mut st, reason, DisabledBy::Operator)
    }

    /// Starts the current generation without evaluating.
    pub fn enable(&self, name: &str) -> Result<Option<u32>> {
        self.service(name)?;
        let _locks = self.locks(name, true, "enable")?;
        let mut st = State::load(&self.state_path(name))?;
        if st.disabled.is_none() {
            return Ok(st.generation);
        }
        st.disabled = None;
        let Some(m) = self.current(name, &st)? else {
            st.save(&self.state_path(name))?;
            return Ok(None);
        };
        exports::publish(&self.config.runtime_dir, name, &m.exports)?;
        systemd::start(&m.units, true).and_then(|()| health_check_run(name, &m.units))?;
        st.save(&self.state_path(name))?;
        Ok(st.generation)
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

    /// Names of declarative services plus manually deployed ones.
    pub fn service_names(&self) -> Result<Vec<String>> {
        let mut names: Vec<String> = self.config.services.keys().cloned().collect();
        for name in self.state_dirs()? {
            if !names.contains(&name) && self.manual_config_path(&name).exists() {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// Declarative services from config.json plus manually deployed ones.
    /// On name collision the declarative definition wins.
    pub fn services(&self) -> Result<BTreeMap<String, (ServiceConfig, Origin)>> {
        self.service_names()?
            .into_iter()
            .map(|name| {
                let svc = self.service(&name)?;
                Ok((name, svc))
            })
            .collect()
    }

    fn service(&self, name: &str) -> Result<(ServiceConfig, Origin)> {
        let (svc, origin) = if let Some(svc) = self.config.services.get(name) {
            (svc.clone(), Origin::Declarative)
        } else {
            let path = self.manual_config_path(name);
            if !path.exists() {
                return Err(Error::UnknownService(name.into()));
            }
            let data = fs::read_to_string(&path)
                .map_err(Error::io(format!("cannot read {}", path.display())))?;
            let svc = serde_json::from_str(&data)
                .map_err(Error::json(format!("corrupt {}", path.display())))?;
            (svc, Origin::Manual)
        };
        validate_output(name, &svc.output)?;
        Ok((svc, origin))
    }

    fn state_dirs(&self) -> Result<Vec<String>> {
        let io = || Error::io(format!("read {}", self.config.state_dir.display()));
        let mut names = Vec::new();
        if !self.local {
            return Ok(names);
        }
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
        let mut names = self.service_names()?;
        for name in self.state_dirs()? {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names.sort();
        let mut result = Vec::new();
        for name in names {
            let (svc, origin) = match self.service(&name) {
                Ok(s) => s,
                Err(Error::UnknownService(_)) => {
                    result.push(ServiceStatus {
                        last_error: Some(
                            "state directory without configuration; remove or redeploy".into(),
                        ),
                        ..ServiceStatus::named(name)
                    });
                    continue;
                }
                Err(e) => {
                    result.push(ServiceStatus {
                        last_error: Some(e.to_string()),
                        ..ServiceStatus::named(name)
                    });
                    continue;
                }
            };
            let st = match State::load(&self.state_path(&name)) {
                Ok(st) => st,
                Err(e) => {
                    result.push(ServiceStatus {
                        flake: svc.flake,
                        origin,
                        last_error: Some(e.to_string()),
                        ..ServiceStatus::named(name)
                    });
                    continue;
                }
            };
            let updating =
                lock::acquire(&self.service_lock(&name), true, false, "status probe").is_err();
            let mut status = ServiceStatus {
                flake: svc.flake,
                origin,
                generation: st.generation,
                pin: st.pin.clone(),
                override_flake: st.override_flake.clone(),
                degraded: st.degraded,
                held: st.hold.as_ref().map(|h| h.reason.clone()),
                disabled: st.disabled.clone(),
                last_error: st.last_error.clone(),
                updating,
                changed: st.changed.clone(),
                ..ServiceStatus::named(name.clone())
            };
            match self.current(&name, &st) {
                Ok(cur) => {
                    status.export_blockers = self.export_blockers(&st, cur.as_ref());
                    if let Some(m) = cur {
                        status.unit_states = systemd::states(&m.units).unwrap_or_default();
                        status.failed_units = status
                            .unit_states
                            .iter()
                            .filter(|s| s.active == "failed")
                            .map(|s| s.unit.clone())
                            .collect();
                        status.missing_providers =
                            exports::unannounced_claims(&m.exports, &self.config.providers_dir);
                        status.units = m.units;
                        status.locked_url = Some(m.flake_url);
                        status.state = m.state;
                    }
                }
                Err(e) => status.last_error = Some(e.to_string()),
            }
            result.push(status);
        }
        Ok(result)
    }

    fn export_blockers(&self, st: &State, current: Option<&Manifest>) -> Vec<String> {
        let Some(m) = current else {
            return vec!["never deployed".into()];
        };
        let mut b = svcstate::blockers(m.state.as_ref(), &m.exports, &self.config.providers_dir);
        if st.degraded {
            b.push("running a degraded (cached) generation".into());
        }
        if let Some(d) = &st.disabled {
            b.push(format!("disabled ({})", d.reason));
        }
        b
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
        Ok(driver::render(&self.config, &entries))
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
        driver_file
            .write_all(expr.as_bytes())
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
            let missing_providers = out
                .as_ref()
                .and_then(|o| fs::read_to_string(o.join("exports.json")).ok())
                .and_then(|data| serde_json::from_str(&data).ok())
                .map(|exports| exports::unannounced_claims(&exports, &self.config.providers_dir))
                .unwrap_or_default();
            results.push(CheckResult {
                name: job.attr,
                drv_path,
                out,
                missing_providers,
            });
        }
        Ok(results)
    }

    /// Closure diff between the running generation and a fresh evaluation.
    /// Read-only: takes no locks and writes no state, like `status`.
    pub fn diff(&self, name: &str, refresh: bool) -> Result<String> {
        let (svc, _) = self.service(name)?;
        let st = State::load(&self.state_path(name))?;
        let old = self.running(name, &st)?.artifact;
        let new = match &svc.prebuilt {
            Some(prebuilt) => prebuilt.clone(),
            None => {
                let opts = CheckOpts {
                    build: true,
                    refresh,
                    ..CheckOpts::default()
                };
                let result = self
                    .check(slice::from_ref(&name.to_string()), &opts)?
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
        validate_name(name)?;
        if self.config.services.contains_key(name) {
            return Err(Error::DeclaredService(name.into()));
        }
        let fresh = !self.service_dir(name).exists();
        {
            let _locks = self.locks(name, !opts.no_wait, "deploy")?;
            write_json_atomic(&self.manual_config_path(name), svc)?;
        }
        let result = self.update(name, opts);
        // Do not keep the registration of a failed first deploy.
        let gens = Generations::new(&self.config.gcroot_dir, name);
        if result.is_err() && fresh && gens.list().map_or(true, |g| g.is_empty()) {
            let _ = fs::remove_dir_all(self.service_dir(name));
        }
        result
    }

    /// Stop a service and delete its generations and state.
    /// Returns the state folders that still hold data.
    pub fn remove(&self, name: &str, purge: bool) -> Result<Vec<PathBuf>> {
        validate_name(name)?;
        if !self.service_dir(name).exists() {
            return Err(Error::NeverDeployed(name.into()));
        }
        let _locks = self.locks(name, true, "remove")?;
        let st = State::load(&self.state_path(name))?;
        let current = self.current(name, &st)?;
        if let Some(m) = &current {
            systemd::remove(&m.units)?;
        }
        exports::unpublish(&self.config.runtime_dir, name)?;
        Generations::new(&self.config.gcroot_dir, name).remove_all()?;
        fs::remove_dir_all(self.service_dir(name))
            .map_err(Error::io(format!("remove state of {name}")))?;
        let mut left = Vec::new();
        for f in current
            .iter()
            .flat_map(|m| &m.state)
            .flat_map(|s| &s.folders)
        {
            let real = transfer::real_path(f);
            if purge {
                let _ = fs::remove_dir_all(&real);
                if f.dynamic {
                    let _ = fs::remove_file(&f.path);
                }
            } else if !transfer::is_empty_dir(&real) {
                left.push(real);
            }
        }
        Ok(left)
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
                self.remove(&name, false)?;
                removed.push(name);
            }
        }
        Ok(removed)
    }

    /// Re-link the units of all deployed services at boot, without evaluation.
    /// Returns the re-linked services and per-service failures.
    pub fn boot(&self) -> Result<(Vec<String>, Failures)> {
        let mut linked = Vec::new();
        let mut failed = Vec::new();
        for name in self.state_dirs()? {
            let relink = |name: &str| -> Result<bool> {
                let st = State::load(&self.state_path(name))?;
                if st.disabled.is_some() {
                    return Ok(false);
                }
                let Some(m) = self.current(name, &st)? else {
                    return Ok(false);
                };
                exports::publish(&self.config.runtime_dir, name, &m.exports)?;
                systemd::start(&m.units, false)?;
                Ok(true)
            };
            match relink(&name) {
                Ok(true) => linked.push(name),
                Ok(false) => {}
                Err(e) => failed.push((name, e)),
            }
        }
        Ok((linked, failed))
    }

    /// Pin to the revision of the active generation.
    pub fn lock_service(&self, name: &str) -> Result<String> {
        self.service(name)?;
        let _locks = self.locks(name, true, "lock")?;
        let mut st = State::load(&self.state_path(name))?;
        let url = self
            .current(name, &st)?
            .ok_or_else(|| Error::NeverDeployed(name.into()))?
            .flake_url;
        st.pin = Some(url.clone());
        st.save(&self.state_path(name))?;
        Ok(url)
    }

    pub fn unlock_service(&self, name: &str) -> Result<()> {
        self.service(name)?;
        let _locks = self.locks(name, true, "unlock")?;
        let mut st = State::load(&self.state_path(name))?;
        st.pin = None;
        st.save(&self.state_path(name))
    }

    pub fn gc(&self, keep_override: Option<u32>) -> Result<Vec<(String, Vec<u32>)>> {
        let _global = lock::acquire(&self.global_lock(), true, true, "gc")?;
        let mut pruned = Vec::new();
        for name in self.service_names()? {
            let keep = match (keep_override, self.service(&name)) {
                (Some(n), _) => n,
                (None, Ok((svc, _))) => svc.keep_generations,
                (None, Err(e)) => {
                    eprintln!("{name}: skipped: {e}");
                    continue;
                }
            };
            let st = match State::load(&self.state_path(&name)) {
                Ok(st) => st,
                Err(e) => {
                    eprintln!("{name}: skipped: {e}");
                    continue;
                }
            };
            let gens =
                Generations::new(&self.config.gcroot_dir, &name).prune(keep, st.generation)?;
            if !gens.is_empty() {
                pruned.push((name, gens));
            }
        }
        Ok(pruned)
    }

    pub fn rollback(&self, name: &str) -> Result<u32> {
        self.service(name)?;
        let _locks = self.locks(name, true, "rollback")?;
        let mut st = State::load(&self.state_path(name))?;
        let gens = Generations::new(&self.config.gcroot_dir, name);
        let current = self.running(name, &st)?;
        let target = *gens
            .list()?
            .iter()
            .rfind(|&&g| Some(g) < st.generation)
            .ok_or_else(|| Error::NoOlderGeneration(name.into()))?;
        let manifest = gens.manifest(target)?;
        exports::publish(&self.config.runtime_dir, name, &manifest.exports)?;
        if let Err(e) = systemd::switch(&current.units, &manifest.units) {
            exports::publish(&self.config.runtime_dir, name, &current.exports)?;
            return Err(e);
        }
        st.changed = Some(Change::now(
            target,
            By::Rollback {
                from: st.generation.unwrap_or(0),
            },
        ));
        st.generation = Some(target);
        st.save(&self.state_path(name))?;
        Ok(target)
    }

    /// Describe what `export` would write. Also the exportability check.
    pub fn export_meta(&self, name: &str) -> Result<(ExportMeta, Units)> {
        let (svc, _) = self.service(name)?;
        let st = State::load(&self.state_path(name))?;
        let current = self.current(name, &st)?;
        let reasons = self.export_blockers(&st, current.as_ref());
        if !reasons.is_empty() {
            return Err(Error::NotTransferable {
                service: name.into(),
                verb: "exported",
                reasons,
            });
        }
        let manifest = current.expect("blockers checked deployment");
        let meta = ExportMeta {
            version: transfer::FORMAT_VERSION,
            flakelet_version: env!("CARGO_PKG_VERSION").into(),
            name: name.into(),
            source_host: transfer::hostname(),
            created: unix_time(),
            flake: svc.flake,
            output: svc.output,
            flake_url: manifest.flake_url,
            flake_rev: manifest.flake_rev,
            settings_hash: manifest.settings_hash,
            state: manifest.state.expect("blockers checked state.json"),
            exports: manifest.exports,
            consistency: "stopped".into(),
        };
        Ok((meta, manifest.units))
    }

    /// Stop the service, collect its state into `out` and leave it
    /// disabled, or with `copy` running again.
    pub fn export(
        &self,
        name: &str,
        out: &Path,
        copy: bool,
        to: Option<&str>,
    ) -> Result<ExportMeta> {
        let _locks = self.locks(name, true, "export")?;
        let (meta, units) = self.export_meta(name)?;
        fs::create_dir_all(&self.config.cache_dir).map_err(Error::io("create cache dir"))?;
        let work =
            tempfile::tempdir_in(&self.config.cache_dir).map_err(Error::io("create export dir"))?;
        let dir = work.path();
        write_json_atomic(&dir.join("meta.json"), &meta)?;
        for d in ["state", "requires"] {
            fs::create_dir(dir.join(d)).map_err(Error::io("create export dir"))?;
        }

        eprintln!("{name}: stopping units");
        systemd::stop(&units)?;
        let collected = (|| {
            if let Some(unit) = &meta.state.dump {
                eprintln!("{name}: running {unit}");
                if !systemd::start_oneshot(unit)? {
                    return Err(Error::OneshotFailed {
                        service: name.into(),
                        unit: unit.clone(),
                    });
                }
            }
            transfer::provider_hooks(&meta.exports, &self.config.providers_dir, dir, false, false)?;
            for (i, folder) in meta.state.folders.iter().enumerate() {
                eprintln!("{name}: archiving {}", folder.path.display());
                transfer::tar_folder(folder, &dir.join(format!("state/{i}.tar")))?;
            }
            Ok(())
        })();
        let packed = collected.and_then(|()| transfer::pack(dir, out));
        if copy || packed.is_err() {
            eprintln!("{name}: starting units");
            let started = systemd::start(&units, true);
            packed?;
            started?;
            return Ok(meta);
        }
        let reason = match to {
            Some(h) => format!("exported to {h}"),
            None => "exported".into(),
        };
        let mut st = State::load(&self.state_path(name))?;
        self.set_disabled(name, &mut st, &reason, DisabledBy::Export)?;
        Ok(meta)
    }

    /// Register (if needed), build, restore state from an export archive
    /// and activate. `settings` are for a newly registered manual entry.
    /// An existing entry keeps its own.
    pub fn import(
        &self,
        archive: &Path,
        name: Option<&str>,
        settings: Option<Value>,
        mut opts: UpdateOpts,
    ) -> Result<(String, UpdateOutcome)> {
        fs::create_dir_all(&self.config.cache_dir).map_err(Error::io("create cache dir"))?;
        let work =
            tempfile::tempdir_in(&self.config.cache_dir).map_err(Error::io("create import dir"))?;
        transfer::unpack(archive, work.path())?;
        let meta = ExportMeta::load(work.path())?;
        let renamed = name.is_some_and(|n| n != meta.name);
        let name = name.unwrap_or(&meta.name).to_string();
        validate_name(&name)?;
        // With --name the folders derive differently, only the build can tell.
        if !renamed {
            self.import_precheck(&name, &meta.state, &meta.exports, !opts.replace)?;
        }

        let fresh = self.service(&name).is_err();
        let locks = self.locks(&name, !opts.no_wait, "import")?;
        if fresh {
            let mut svc = ServiceConfig {
                flake: meta.flake.clone(),
                output: meta.output.clone(),
                ..ServiceConfig::default()
            };
            if let Some(s) = settings {
                svc.settings = s;
            }
            write_json_atomic(&self.manual_config_path(&name), &svc)?;
        }
        let mut st = State::load(&self.state_path(&name))?;
        st.pin = Some(meta.flake_url.clone());
        st.save(&self.state_path(&name))?;
        drop(locks);
        opts.restore_from = Some(work.path().to_path_buf());
        opts.force = true;
        let result = self.update(&name, opts);
        if fresh && !matches!(result, Ok(UpdateOutcome::Updated { .. })) {
            let _ = Generations::new(&self.config.gcroot_dir, &name).remove_all();
            let _ = fs::remove_dir_all(self.service_dir(&name));
        }
        Ok((name, result?))
    }

    /// Host-side conditions for restoring `state` here.
    fn import_precheck(
        &self,
        name: &str,
        state: &StateInfo,
        exports: &Value,
        need_empty: bool,
    ) -> Result<()> {
        let mut reasons = svcstate::blockers(Some(state), exports, &self.config.providers_dir);
        for f in &state.folders {
            let real = transfer::real_path(f);
            if need_empty && !transfer::is_empty_dir(&real) {
                reasons.push(format!("{} is not empty (see --replace)", real.display()));
            }
            if !f.dynamic && !transfer::user_exists(&f.user) {
                reasons.push(format!("user '{}' does not exist on this host", f.user));
            }
        }
        if reasons.is_empty() {
            Ok(())
        } else {
            Err(Error::NotTransferable {
                service: name.into(),
                verb: "imported",
                reasons,
            })
        }
    }

    /// Between build and activation of an import: put the archived state
    /// in place. The entry is disabled meanwhile so a crash starts nothing.
    fn restore(
        &self,
        name: &str,
        st: &mut State,
        artifact: &Artifact,
        dir: &Path,
        replace: bool,
    ) -> Result<()> {
        let meta = ExportMeta::load(dir)?;
        let Some(target) = &artifact.state else {
            return Err(Error::NotTransferable {
                service: name.into(),
                verb: "imported",
                reasons: vec!["evaluation produced no state.json".into()],
            });
        };
        // state/<i>.tar is positional, so the evaluated folders must equal
        // the archived ones modulo the entry name.
        let mut reasons = Vec::new();
        let renamed: Vec<PathBuf> = meta
            .state
            .folders
            .iter()
            .map(|f| transfer::rename_folder(&f.path, &meta.name, name))
            .collect();
        if target.paths() != renamed.iter().map(PathBuf::as_path).collect::<Vec<_>>()
            || target.dump.is_some() != meta.state.dump.is_some()
            || target.restore.is_some() != meta.state.restore.is_some()
        {
            reasons.push(format!(
                "evaluated state {:?} does not match archive {:?}",
                target.paths(),
                meta.state.paths()
            ));
        }
        if exports::claims(&artifact.exports) != exports::claims(&meta.exports) {
            reasons.push("evaluated requires.* claims differ from archive".into());
        }
        if !reasons.is_empty() {
            return Err(Error::NotTransferable {
                service: name.into(),
                verb: "imported",
                reasons,
            });
        }
        self.import_precheck(name, target, &artifact.exports, !replace)?;

        let reason = format!("import from {} did not finish", meta.source_host);
        self.set_disabled(name, st, &reason, DisabledBy::Import)?;
        let run = || -> Result<()> {
            if replace {
                for folder in &target.folders {
                    transfer::clear_dir(&transfer::real_path(folder));
                }
            }
            for (i, folder) in target.folders.iter().enumerate() {
                eprintln!("{name}: restoring {}", folder.path.display());
                transfer::untar_folder(folder, &dir.join(format!("state/{i}.tar")))?;
            }
            transfer::provider_hooks(
                &artifact.exports,
                &self.config.providers_dir,
                dir,
                true,
                replace,
            )?;
            if let Some(unit) = &target.restore {
                eprintln!("{name}: running {unit}");
                systemd::load(&artifact.units)?;
                if !systemd::start_oneshot(unit)? {
                    return Err(Error::OneshotFailed {
                        service: name.into(),
                        unit: unit.clone(),
                    });
                }
            }
            Ok(())
        };
        let result = run();
        if result.is_err() {
            // Only drops what was just extracted.
            for folder in &target.folders {
                transfer::clear_dir(&transfer::real_path(folder));
            }
            let _ = systemd::stop(&artifact.units);
        }
        result
    }

    pub fn update(&self, name: &str, opts: UpdateOpts) -> Result<UpdateOutcome> {
        let (svc, origin) = self.service(name)?;
        if opts.flake.is_some() && svc.prebuilt.is_some() {
            return Err(Error::OverrideWithPrebuilt(name.into()));
        }
        let _locks = self.locks(name, !opts.no_wait, "update")?;
        let state_path = self.state_path(name);
        let mut st = State::load(&state_path)?;
        // Import is how a failed import is retried.
        if let (Some(d), None) = (&st.disabled, &opts.restore_from) {
            return Ok(UpdateOutcome::Disabled {
                reason: d.reason.clone(),
            });
        }
        st.origin = origin;
        st.override_flake = opts.flake.clone();

        match self.try_update(name, &svc, &mut st, &opts) {
            Ok(outcome) => {
                st.save(&state_path)?;
                Ok(outcome)
            }
            Err(err) => {
                let msg = err.to_string();
                st.last_error = Some(msg.clone());
                // Nothing to fall back to on a first deploy.
                if opts.offline_fallback && err.is_network_error() && st.generation.is_some() {
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
        opts: &UpdateOpts,
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
                return Err(Error::MissingArtifact(prebuilt.clone()));
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
            let flake_ref =
                effective_flake_ref(opts.flake.as_deref(), st.pin.as_deref(), &svc.flake)
                    .to_string();
            if opts.flake.is_some() {
                eprintln!(
                    "{name}: warning: testing override, the next update without \
                     --flake (including host activations) reverts to {}",
                    st.pin.as_deref().unwrap_or(&svc.flake)
                );
            }
            eprintln!("{name}: resolving {flake_ref}");
            let locked = nix.locked_url(&flake_ref, !opts.no_refresh)?;
            let nixpkgs = resolve_overrides(name, svc, &nix, !opts.no_refresh)?;
            self.evaluate(name, svc, &nix, &locked, nixpkgs.as_ref(), &settings_hash)?
        };

        if !opts.force {
            if let Some(hold) = st.held_for(&artifact.out) {
                return Ok(UpdateOutcome::Held {
                    reason: hold.reason.clone(),
                });
            }
        }
        read_contents(name, &mut artifact)?;
        self.check_conflicts(name, &artifact)?;

        let mut current = self.current(name, st)?;
        if let Some(dir) = &opts.restore_from {
            self.restore(name, st, &artifact, dir, opts.replace)?;
            // State was replaced, nothing to roll back to.
            current = None;
        } else if !opts.force && current.as_ref().is_some_and(|m| m.artifact == artifact.out) {
            st.degraded = false;
            st.last_error = None;
            return Ok(UpdateOutcome::UpToDate);
        }
        self.activate(
            name,
            svc,
            st,
            current.as_ref(),
            artifact,
            soft_refs,
            &opts.by,
        )
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
    /// rolling back to `previous` if activation fails.
    #[allow(clippy::too_many_arguments)]
    fn activate(
        &self,
        name: &str,
        svc: &ServiceConfig,
        st: &mut State,
        previous: Option<&Manifest>,
        artifact: Artifact,
        soft_refs: Vec<String>,
        by: &By,
    ) -> Result<UpdateOutcome> {
        let units = artifact.units.clone();
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
            artifact: artifact.out.clone(),
            exports: artifact.exports.clone(),
            state: artifact.state.clone(),
            created: unix_time(),
        };
        exports::provision(
            name,
            &artifact.exports,
            &self.config.providers_dir,
            &self.service_dir(name),
        )?;
        let generation = gens.create(&manifest, &extra_roots)?;

        eprintln!("{name}: activating generation {generation}");
        let previous_units = previous.map(|m| m.units.clone()).unwrap_or_default();
        // Publish first so level-triggered providers without a provision
        // hook can act before the readiness probe needs them.
        exports::publish(&self.config.runtime_dir, name, &artifact.exports)?;
        let result =
            systemd::switch(&previous_units, &units).and_then(|()| health_check_run(name, &units));

        if let Err(err) = result {
            let reason = err.to_string();
            // A generation that never ran is not a rollback target.
            gens.remove(generation)?;
            match previous {
                Some(m) => exports::publish(&self.config.runtime_dir, name, &m.exports)?,
                None => exports::unpublish(&self.config.runtime_dir, name)?,
            }
            systemd::switch(&units, &previous_units).map_err(|e| Error::RollbackFailed {
                service: name.into(),
                source: Box::new(e),
            })?;
            st.hold = Some(Hold {
                reason: reason.clone(),
                artifact: artifact.out,
            });
            st.last_error = Some(reason.clone());
            return Ok(UpdateOutcome::RolledBack { reason });
        }

        st.changed = Some(Change::now(generation, by.clone()));
        st.generation = Some(generation);
        st.hold = None;
        st.disabled = None;
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
            let Some(m) = self.current(&other, &st)? else {
                continue;
            };
            if let Some(unit) = artifact.units.keys().find(|u| m.units.contains_key(*u)) {
                return Err(Error::UnitConflict {
                    service: name.into(),
                    unit: unit.clone(),
                    owner: other,
                });
            }
            exports::check_port_conflicts(name, &artifact.exports, &other, &m.exports)?;
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
    exports: Value,
    state: Option<crate::svcstate::StateInfo>,
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

const UNIT_TYPES: &[&str] = &["service", "socket", "target", "timer", "path"];

/// Unit dirs owned by the host; a runtime symlink would silently shadow them.
const HOST_UNIT_DIRS: &[&str] = &[
    "/etc/systemd/system",
    "/usr/lib/systemd/system",
    "/lib/systemd/system",
];

/// Enforce the service contract on unit names before anything is activated:
/// names start with the entry name, only the supported unit types, and no
/// shadowing of units the host already owns.
/// The output attribute path is spliced into the driver expression;
/// restrict it to dotted identifiers to rule out Nix code injection.
fn validate_output(service: &str, output: &str) -> Result<()> {
    let ident = |s: &str| {
        let mut chars = s.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '\''))
    };
    if !output.is_empty() && output.split('.').all(ident) {
        return Ok(());
    }
    Err(Error::InvalidOutput {
        service: service.into(),
        output: output.into(),
    })
}

fn validate_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok = name.len() <= 128
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        // No dots. The name is a driver attribute and nix-eval-jobs quotes dotted ones.
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidServiceName(name.into()))
    }
}

fn validate_units(name: &str, units: &Units, host_dirs: &[PathBuf]) -> Result<()> {
    for unit in units.keys() {
        let ok = unit
            .rsplit_once('.')
            .is_some_and(|(base, suffix)| match suffix {
                _ if !UNIT_TYPES.contains(&suffix) => false,
                _ => {
                    // `prefix` or `prefix@instance`, prefix being name or name-*
                    let prefix = base.split_once('@').map_or(base, |(p, _)| p);
                    base.matches('@').count() <= 1
                        && (prefix == name || prefix.starts_with(&format!("{name}-")))
                }
            });
        if !ok {
            return Err(Error::InvalidUnitName {
                service: name.into(),
                unit: unit.clone(),
            });
        }
        // An instance is also shadowed by a host-owned template.
        let template = systemd::template_of(unit);
        for candidate in std::iter::once(unit.as_str()).chain(template.as_deref()) {
            if let Some(dir) = host_dirs.iter().find(|d| d.join(candidate).exists()) {
                return Err(Error::HostUnitConflict {
                    service: name.into(),
                    unit: unit.clone(),
                    path: dir.join(candidate),
                });
            }
        }
    }
    Ok(())
}

/// Read units/ and the optional exports from a built driver output.
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
    let host_dirs: Vec<PathBuf> = HOST_UNIT_DIRS.iter().map(PathBuf::from).collect();
    validate_units(name, &artifact.units, &host_dirs)?;
    // Generators travel with the units so switch and remove handle them.
    let gen_dir = artifact.out.join("generators");
    for entry in fs::read_dir(&gen_dir).into_iter().flatten().flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if !file.starts_with(&format!("{name}-")) || file.contains('.') {
            return Err(Error::InvalidUnitName {
                service: name.into(),
                unit: file,
            });
        }
        let target = fs::canonicalize(entry.path())
            .map_err(Error::io(format!("read {}", gen_dir.display())))?;
        artifact.units.insert(file, target);
    }
    artifact.exports = match fs::read_to_string(artifact.out.join("exports.json")) {
        Ok(data) => serde_json::from_str(&data)
            .map_err(Error::json(format!("corrupt exports.json of {name}")))?,
        Err(e) if e.kind() == ErrorKind::NotFound => Value::Null,
        Err(e) => return Err(Error::io(format!("read exports.json of {name}"))(e)),
    };
    artifact.state = match fs::read_to_string(artifact.out.join("state.json")) {
        Ok(data) => Some(
            serde_json::from_str(&data)
                .map_err(Error::json(format!("corrupt state.json of {name}")))?,
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(Error::io(format!("read state.json of {name}"))(e)),
    };
    Ok(())
}

/// Start the optional `<name>-health.service` probe, then check for failed
/// units. Readiness/liveness beyond that is the units' own business.
fn health_check_run(name: &str, units: &Units) -> Result<()> {
    let probe = format!("{name}-health.service");
    if units.contains_key(&probe) && !systemd::start_oneshot(&probe)? {
        return Err(Error::HealthCheckFailed {
            service: name.into(),
            unit: probe,
        });
    }
    if let Some(unit) = systemd::failed(units)?.into_iter().next() {
        return Err(Error::UnitFailed {
            service: name.into(),
            unit,
        });
    }
    Ok(())
}

/// Resolve input_overrides to locked references. Only 'nixpkgs' is supported:
/// it is the one dependency flakelet hands in, while the service flake's own
/// inputs would need their lock rewritten, which builtins.getFlake cannot do
/// purely.
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
    fn disabled_entries_are_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let svc = ServiceConfig {
            flake: "github:me/web".into(),
            ..Default::default()
        };
        write_json_atomic(&mgr.manual_config_path("web"), &svc).unwrap();
        mgr.disable("web", "maintenance").unwrap();
        let st = State::load(&mgr.state_path("web")).unwrap();
        assert_eq!(st.disabled.as_ref().unwrap().by, DisabledBy::Operator);

        let outcome = mgr.update("web", UpdateOpts::default()).unwrap();
        assert_eq!(
            outcome,
            UpdateOutcome::Disabled {
                reason: "maintenance".into()
            }
        );
        assert!(matches!(mgr.rollback("web"), Err(Error::Disabled { .. })));
        assert!(mgr.boot().unwrap().0.is_empty());

        assert_eq!(mgr.enable("web").unwrap(), None);
        assert!(State::load(&mgr.state_path("web"))
            .unwrap()
            .disabled
            .is_none());
    }

    #[test]
    fn flake_ref_precedence() {
        let over = "github:me/app/test-branch";
        let pin = "github:me/app/abc123";
        assert_eq!(
            effective_flake_ref(Some(over), Some(pin), "github:me/app"),
            over
        );
        assert_eq!(effective_flake_ref(None, Some(pin), "github:me/app"), pin);
        assert_eq!(
            effective_flake_ref(None, None, "github:me/app"),
            "github:me/app"
        );
    }

    #[test]
    fn service_names_are_validated() {
        for ok in ["web", "my-app", "a_c2"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "../evil",
            "a b",
            "a/b",
            ".hidden",
            "a.b",
            "-x",
            "a\nb",
            &"a".repeat(129),
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn output_attrpaths_are_validated() {
        for ok in ["flakelet", "flakelets.default", "a.b_c'.d-e"] {
            assert!(validate_output("web", ok).is_ok(), "{ok}");
        }
        for bad in ["", "a..b", "a.", "1a", "a) {}; evil = (x", "a b"] {
            assert!(validate_output("web", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn unit_names_are_validated() {
        let tmp = tempfile::tempdir().unwrap();
        let host_dir = tmp.path().to_path_buf();
        let units = |names: &[&str]| -> Units {
            names
                .iter()
                .map(|n| (n.to_string(), "/nix/store/x".into()))
                .collect()
        };

        let ok = units(&[
            "web.service",
            "web.socket",
            "web-worker.timer",
            "web-pre.target",
            "web@.service",
            "web-agent@.socket",
            "web-agent@1.socket",
        ]);
        assert!(validate_units("web", &ok, slice::from_ref(&host_dir)).is_ok());

        for bad in [
            "webfoo.service",
            "other.service",
            "web.mount",
            "web",
            "web-x.swap",
            "web-a@b@c.service",
            "webx@.service",
        ] {
            assert!(
                matches!(
                    validate_units("web", &units(&[bad]), &[]),
                    Err(Error::InvalidUnitName { .. })
                ),
                "{bad} should be rejected"
            );
        }

        // A unit the host already owns must not be shadowed.
        fs::write(host_dir.join("web.service"), "").unwrap();
        assert!(matches!(
            validate_units("web", &units(&["web.service"]), slice::from_ref(&host_dir)),
            Err(Error::HostUnitConflict { .. })
        ));
        // An instance would be backed by a host template.
        fs::write(host_dir.join("web-agent@.socket"), "").unwrap();
        assert!(matches!(
            validate_units("web", &units(&["web-agent@1.socket"]), &[host_dir]),
            Err(Error::HostUnitConflict { .. })
        ));
    }

    #[test]
    fn foreign_unit_names_and_ports_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let gen = Generations::new(&mgr.config.gcroot_dir, "other")
            .create(
                &Manifest {
                    version: 1,
                    units: Units::from([("web.service".into(), "/nix/store/x".into())]),
                    flake_url: String::new(),
                    flake_rev: String::new(),
                    settings_hash: String::new(),
                    driver: "/nix/store/d".into(),
                    artifact: "/nix/store/a".into(),
                    exports: serde_json::json!({ "ports": { "http": { "port": 80 } } }),
                    state: None,
                    created: 0,
                },
                &[],
            )
            .unwrap();
        State {
            generation: Some(gen),
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
