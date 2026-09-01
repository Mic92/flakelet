//! All units of an entry are stopped and started as a whole. Without
//! per-unit diffing sockets, timers and their services never mix
//! generations and a stale `failed` state cannot survive a switch.
//! The exception is `X-RestartIfChanged=false`, for units whose running
//! instances must drain (build agents).
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const RUNTIME_UNIT_DIR: &str = "/run/systemd/system";
const GENERATOR_DIR: &str = "/run/systemd/system-generators";

/// Map of unit name -> unit file store path. Entries without a `.` are
/// systemd generators.
pub type Units = BTreeMap<String, PathBuf>;

fn is_generator(unit: &str) -> bool {
    !unit.contains('.')
}

fn link_dir(unit: &str) -> &'static Path {
    Path::new(if is_generator(unit) {
        GENERATOR_DIR
    } else {
        RUNTIME_UNIT_DIR
    })
}

/// `foo@.service` cannot be started or queried itself, only linked.
fn is_template(unit: &str) -> bool {
    unit.contains("@.")
}

/// `foo@1.service` -> `foo@.service`.
pub(crate) fn template_of(unit: &str) -> Option<String> {
    let (base, suffix) = unit.rsplit_once('.')?;
    let (prefix, instance) = base.split_once('@')?;
    (!instance.is_empty()).then(|| format!("{prefix}@.{suffix}"))
}

/// Replace `old` by `new`. Everything of `old` is stopped and unlinked
/// before anything of `new` starts, so ports and sockets are free.
pub fn switch(old: &Units, new: &Units) -> Result<()> {
    // Running X-RestartIfChanged=false services present in both keep
    // running, and so does their socket, which could not restart while
    // they do.
    let mut keep = Vec::new();
    for (unit, path) in new.iter().filter(|(u, _)| u.ends_with(".service")) {
        if !old.contains_key(unit) || UnitFile::read(unit, path)?.restart_if_changed {
            continue;
        }
        let names = if is_template(unit) {
            instances(unit)?
        } else {
            vec![unit.clone()]
        };
        for (service, _) in show(&names, "ActiveState")?
            .into_iter()
            .filter(|(_, s)| s != "inactive" && s != "failed")
        {
            keep.push(service.replace(".service", ".socket"));
            keep.push(service);
        }
    }
    remove_except(old, &keep)?;
    start(new, true)
}

/// Stop all loaded units. Links stay in place.
pub fn stop(units: &Units) -> Result<()> {
    stop_except(units, &[])
}

fn stop_except(units: &Units, keep: &[String]) -> Result<()> {
    let stop: Vec<String> = loaded(units)?
        .into_iter()
        .filter(|u| !keep.contains(u))
        .collect();
    // Triggers first: in a single job systemd stops the service before its
    // socket, and a pending connection re-activates it in between.
    let (triggers, rest): (Vec<_>, Vec<_>) = stop
        .iter()
        .map(String::as_str)
        .partition(|u| u.ends_with(".socket") || u.ends_with(".timer") || u.ends_with(".path"));
    for batch in [triggers, rest] {
        if !batch.is_empty() {
            let mut args = vec!["stop"];
            args.extend(batch);
            systemctl(&args)?;
        }
    }
    Ok(())
}

/// Stop, disable and unlink all units.
pub fn remove(units: &Units) -> Result<()> {
    remove_except(units, &[])
}

fn remove_except(units: &Units, keep: &[String]) -> Result<()> {
    if units.is_empty() {
        return Ok(());
    }
    stop_except(units, keep)?;
    // Kept units are disabled too so WantedBy reflects the new generation.
    let loaded = loaded(units)?;
    if !loaded.is_empty() {
        let mut args = vec!["disable", "--runtime"];
        args.extend(loaded.iter().map(String::as_str));
        systemctl(&args)?;
        let _ = Command::new("systemctl")
            .arg("reset-failed")
            .args(&loaded)
            .output();
    }
    // A running socket whose unit file is missing during a daemon-reload
    // loses its listening fd for good, so kept units stay linked and are
    // relinked in place by start().
    let kept_files: Vec<String> = keep
        .iter()
        .map(|u| template_of(u).unwrap_or_else(|| u.clone()))
        .collect();
    for unit in units.keys().filter(|u| !kept_files.contains(u)) {
        match fs::remove_file(link_dir(unit).join(unit)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(Error::io(format!("unlink unit {unit}"))(e))
            }
            _ => {}
        }
    }
    systemctl(&["daemon-reload"])
}

/// Link all units, enable and start those with an [Install] section.
/// Units without one are pulled in on demand (socket, timer).
/// At boot `block` is false so the start jobs join the running boot
/// transaction instead of deadlocking on it.
pub fn start(units: &Units, block: bool) -> Result<()> {
    if units.is_empty() {
        return Ok(());
    }
    load(units)?;
    // A failure from before this start must not fail the health check.
    let _ = Command::new("systemctl")
        .arg("reset-failed")
        .args(concrete(units)?)
        .output();

    let mut installed = Vec::new();
    let mut generated = Vec::new();
    for (unit, path) in units {
        if is_generator(unit) || !UnitFile::read(unit, path)?.install {
            continue;
        }
        if !is_template(unit) {
            installed.push(unit.clone());
            continue;
        }
        // Instances a generator hooked into a target during the reload.
        for i in instances(unit)?
            .into_iter()
            .filter(|i| !units.contains_key(i))
        {
            if !show(std::slice::from_ref(&i), "WantedBy,RequiredBy")?.is_empty() {
                generated.push(i);
            }
        }
    }
    if !installed.is_empty() {
        let mut enable = vec!["enable", "--runtime"];
        enable.extend(installed.iter().map(String::as_str));
        systemctl(&enable)?;
    }
    installed.extend(generated);
    if installed.is_empty() {
        return Ok(());
    }
    let mut start = vec!["start"];
    if !block {
        start.push("--no-block");
    }
    start.extend(installed.iter().map(String::as_str));
    systemctl(&start).map_err(|e| match (e, failed(units)) {
        (Error::Command { program, args, .. }, Ok(f)) if !f.is_empty() => Error::Command {
            program,
            args,
            stderr: format!("failed units: {}. See journalctl -u {}", f.join(" "), f[0]),
        },
        (e, _) => e,
    })
}

/// Link the unit files and make systemd load them, without starting.
pub fn load(units: &Units) -> Result<()> {
    for (unit, path) in units {
        link(unit, path)?;
    }
    systemctl(&["daemon-reload"])
}

/// Unit names systemd can be asked about: templates replaced by the
/// instances it currently knows.
fn concrete(units: &Units) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for unit in units.keys() {
        if is_generator(unit) {
            continue;
        }
        if is_template(unit) {
            for i in instances(unit)? {
                if !units.contains_key(&i) {
                    out.push(i);
                }
            }
        } else {
            out.push(unit.clone());
        }
    }
    Ok(out)
}

fn loaded(units: &Units) -> Result<Vec<String>> {
    Ok(show(&concrete(units)?, "LoadState")?
        .into_iter()
        .filter(|(_, s)| s != "not-found")
        .map(|(u, _)| u)
        .collect())
}

struct UnitFile {
    install: bool,
    restart_if_changed: bool,
}

impl UnitFile {
    fn read(unit: &str, path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(Error::io(format!("read unit {unit}")))?;
        Ok(Self {
            install: text.contains("[Install]"),
            restart_if_changed: !text.lines().any(|l| l.trim() == "X-RestartIfChanged=false"),
        })
    }
}

/// Instances of a template systemd currently has loaded.
fn instances(template: &str) -> Result<Vec<String>> {
    let pattern = template.replacen("@.", "@*.", 1);
    let out = Command::new("systemctl")
        .args(["list-units", "--all", "--plain", "--no-legend", &pattern])
        .output()
        .map_err(|source| Error::Spawn {
            program: "systemctl".into(),
            source,
        })?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_owned)
        .collect())
}

/// Start a oneshot probe unit; Ok(false) when the start job failed.
pub fn start_oneshot(unit: &str) -> Result<bool> {
    let status = Command::new("systemctl")
        .args(["start", unit])
        .status()
        .map_err(|source| Error::Spawn {
            program: "systemctl".into(),
            source,
        })?;
    Ok(status.success())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UnitState {
    pub unit: String,
    /// ActiveState, e.g. active, inactive, failed.
    pub active: String,
    /// SubState, e.g. running, listening, dead.
    pub sub: String,
}

/// State of every concrete unit of the service, template instances included.
pub fn states(units: &Units) -> Result<Vec<UnitState>> {
    let names = concrete(units)?;
    let active = show(&names, "ActiveState")?;
    let sub = show(&names, "SubState")?;
    Ok(active
        .into_iter()
        .zip(sub)
        .map(|((unit, active), (_, sub))| UnitState { unit, active, sub })
        .collect())
}

/// Units of the service that are in failed state.
pub fn failed(units: &Units) -> Result<Vec<String>> {
    Ok(states(units)?
        .into_iter()
        .filter(|s| s.active == "failed")
        .map(|s| s.unit)
        .collect())
}

fn show(units: &[String], property: &str) -> Result<Vec<(String, String)>> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let out = Command::new("systemctl")
        .args(["show", "--value", "--property", property])
        .args(units)
        .output()
        .map_err(|source| Error::Spawn {
            program: "systemctl".into(),
            source,
        })?;
    // One value per unit in argument order, separated by blank lines.
    let text = String::from_utf8_lossy(&out.stdout);
    let states = text.lines().filter(|l| !l.is_empty());
    Ok(units
        .iter()
        .zip(states)
        .map(|(u, s)| (u.clone(), s.to_string()))
        .collect())
}

fn link(unit: &str, target: &PathBuf) -> Result<()> {
    let context = || format!("link unit {unit}");
    let dir = link_dir(unit);
    fs::create_dir_all(dir).map_err(Error::io(context()))?;
    let tmp = dir.join(format!(".{unit}.tmp"));
    let _ = fs::remove_file(&tmp);
    symlink(target, &tmp).map_err(Error::io(context()))?;
    fs::rename(&tmp, dir.join(unit)).map_err(Error::io(context()))
}

fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|source| Error::Spawn {
            program: "systemctl".into(),
            source,
        })?;
    if !out.status.success() {
        return Err(Error::Command {
            program: "systemctl".into(),
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_names() {
        assert!(is_template("web-agent@.socket"));
        assert!(!is_template("web-agent@1.socket"));
        assert_eq!(
            template_of("web-agent@1.socket").as_deref(),
            Some("web-agent@.socket")
        );
        assert_eq!(template_of("web-agent@.socket"), None);
        assert_eq!(template_of("web.service"), None);
    }
}
