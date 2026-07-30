use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const RUNTIME_UNIT_DIR: &str = "/run/systemd/system";

/// Map of unit name -> unit file store path.
pub type Units = BTreeMap<String, PathBuf>;

/// Switch a service from `old` to `new` units: link + reload + restart/start,
/// then stop and unlink units that disappeared.
pub fn switch(old: &Units, new: &Units) -> Result<()> {
    if old == new {
        return Ok(());
    }
    for (unit, path) in new {
        link(unit, path)?;
    }
    systemctl(&["daemon-reload"])?;
    for (unit, path) in new {
        if old.get(unit) == Some(path) {
            continue;
        }
        systemctl(&["enable", "--runtime", unit])?;
        let verb = if old.contains_key(unit) {
            "restart"
        } else {
            "start"
        };
        systemctl(&[verb, unit])?;
    }
    let gone: Vec<&str> = old
        .keys()
        .filter(|u| !new.contains_key(*u))
        .map(String::as_str)
        .collect();
    for unit in &gone {
        systemctl(&["disable", "--runtime", "--now", unit])?;
        fs::remove_file(Path::new(RUNTIME_UNIT_DIR).join(unit))
            .map_err(Error::io(format!("unlink unit {unit}")))?;
    }
    if !gone.is_empty() {
        systemctl(&["daemon-reload"])?;
    }
    Ok(())
}

/// Stop a service and remove all its unit links.
pub fn remove(units: &Units) -> Result<()> {
    switch(units, &Units::new())
}

/// Re-link units at boot without starting anything; systemd targets pull them in.
pub fn relink(units: &Units) -> Result<()> {
    for (unit, path) in units {
        link(unit, path)?;
    }
    systemctl(&["daemon-reload"])?;
    for unit in units.keys() {
        systemctl(&["enable", "--runtime", unit])?;
    }
    Ok(())
}

/// First unit of the service that is in failed state, if any.
pub fn any_failed(units: &Units) -> Result<Option<String>> {
    for unit in units.keys() {
        // `is-failed --quiet` exits 0 iff the unit is failed.
        let status = Command::new("systemctl")
            .args(["is-failed", "--quiet", unit])
            .status()
            .map_err(|source| Error::Spawn {
                program: "systemctl".into(),
                source,
            })?;
        if status.success() {
            return Ok(Some(unit.clone()));
        }
    }
    Ok(None)
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
