//! All units of an entry are stopped and started as a whole. Without
//! per-unit diffing sockets, timers and their services never mix
//! generations and a stale `failed` state cannot survive a switch.
//! The exception is `X-RestartIfChanged=false`, for units whose running
//! instances must drain (build agents).
use crate::error::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};
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
    // Units present in both with X-RestartIfChanged=false keep running,
    // and so does their socket, which could not restart while they do.
    let mut keep = BTreeSet::new();
    for (unit, path) in new.iter().filter(|(u, _)| !is_generator(u)) {
        if old.contains_key(unit) && !UnitFile::read(unit, path)?.restart_if_changed {
            let socket = unit.replace(".service", ".socket");
            if new.contains_key(&socket) {
                keep.insert(socket);
            }
            keep.insert(unit.clone());
        }
    }
    let stale: Units = old
        .iter()
        .filter(|(u, _)| !keep.contains(*u) && !template_of(u).is_some_and(|t| keep.contains(&t)))
        .map(|(u, p)| (u.clone(), p.clone()))
        .collect();
    remove(&stale)?;
    start(new, true)
}

/// Stop all loaded units in one job so sockets and timers cannot
/// re-trigger the service. Links stay in place.
pub fn stop(units: &Units) -> Result<()> {
    let loaded = loaded(units)?;
    if loaded.is_empty() {
        return Ok(());
    }
    let mut args = vec!["stop"];
    args.extend(loaded.iter().map(String::as_str));
    systemctl(&args)
}

/// Stop, disable and unlink all units.
pub fn remove(units: &Units) -> Result<()> {
    if units.is_empty() {
        return Ok(());
    }
    stop(units)?;
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
    for unit in units.keys() {
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

    // Instances of an installable template count too, including those a
    // generator hooked in during the reload above.
    let mut installed = Vec::new();
    for (unit, path) in units {
        if is_generator(unit) || !UnitFile::read(unit, path)?.install {
            continue;
        }
        if is_template(unit) {
            installed.extend(
                instances(unit)?
                    .into_iter()
                    .filter(|i| !units.contains_key(i)),
            );
        } else {
            installed.push(unit.clone());
        }
    }
    if installed.is_empty() {
        return Ok(());
    }
    let mut enable = vec!["enable", "--runtime"];
    enable.extend(installed.iter().map(String::as_str));
    systemctl(&enable)?;
    let mut start = vec!["start"];
    if !block {
        start.push("--no-block");
    }
    start.extend(installed.iter().map(String::as_str));
    systemctl(&start)
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

/// Units of the service that are in failed state.
pub fn failed(units: &Units) -> Result<Vec<String>> {
    Ok(show(&concrete(units)?, "ActiveState")?
        .into_iter()
        .filter(|(_, s)| s == "failed")
        .map(|(u, _)| u)
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
