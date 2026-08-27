//! All units of an entry are stopped and started as a whole. Without
//! per-unit diffing sockets, timers and their services never mix
//! generations and a stale `failed` state cannot survive a switch.
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const RUNTIME_UNIT_DIR: &str = "/run/systemd/system";

/// Map of unit name -> unit file store path.
pub type Units = BTreeMap<String, PathBuf>;

/// Replace `old` by `new`. Everything of `old` is stopped and unlinked
/// before anything of `new` starts, so ports and sockets are free.
pub fn switch(old: &Units, new: &Units) -> Result<()> {
    remove(old)?;
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
        match fs::remove_file(Path::new(RUNTIME_UNIT_DIR).join(unit)) {
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
    let mut reset = vec!["reset-failed"];
    reset.extend(units.keys().map(String::as_str));
    let _ = Command::new("systemctl").args(&reset).output();

    let mut installed = Vec::new();
    for (unit, path) in units {
        if has_install(unit, path)? {
            installed.push(unit.as_str());
        }
    }
    if installed.is_empty() {
        return Ok(());
    }
    let mut enable = vec!["enable", "--runtime"];
    enable.extend(&installed);
    systemctl(&enable)?;
    let mut start = vec!["start"];
    if !block {
        start.push("--no-block");
    }
    start.extend(&installed);
    systemctl(&start)
}

/// Link the unit files and make systemd load them, without starting.
pub fn load(units: &Units) -> Result<()> {
    for (unit, path) in units {
        link(unit, path)?;
    }
    systemctl(&["daemon-reload"])
}

fn loaded(units: &Units) -> Result<Vec<String>> {
    Ok(show(units, "LoadState")?
        .into_iter()
        .filter(|(_, s)| s != "not-found")
        .map(|(u, _)| u)
        .collect())
}

fn has_install(unit: &str, path: &PathBuf) -> Result<bool> {
    let text = fs::read_to_string(path).map_err(Error::io(format!("read unit {unit}")))?;
    Ok(text.contains("[Install]"))
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
    Ok(show(units, "ActiveState")?
        .into_iter()
        .filter(|(_, s)| s == "failed")
        .map(|(u, _)| u)
        .collect())
}

fn show(units: &Units, property: &str) -> Result<Vec<(String, String)>> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let out = Command::new("systemctl")
        .args(["show", "--value", "--property", property])
        .args(units.keys())
        .output()
        .map_err(|source| Error::Spawn {
            program: "systemctl".into(),
            source,
        })?;
    // One value per unit in argument order, separated by blank lines.
    let text = String::from_utf8_lossy(&out.stdout);
    let states = text.lines().filter(|l| !l.is_empty());
    Ok(units
        .keys()
        .zip(states)
        .map(|(u, s)| (u.clone(), s.to_string()))
        .collect())
}

fn link(unit: &str, target: &PathBuf) -> Result<()> {
    let context = || format!("link unit {unit}");
    let dir = Path::new(RUNTIME_UNIT_DIR);
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
