use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const RUNTIME_UNIT_DIR: &str = "/run/systemd/system";

/// Map of unit name -> unit file store path.
pub type Units = BTreeMap<String, PathBuf>;

/// `foo@.service` cannot be started or enabled itself, only linked.
fn is_template(unit: &str) -> bool {
    unit.contains("@.")
}

/// `foo@1.service` -> `foo@.service`.
pub(crate) fn template_of(unit: &str) -> Option<String> {
    let (base, suffix) = unit.rsplit_once('.')?;
    let (prefix, instance) = base.split_once('@')?;
    (!instance.is_empty()).then(|| format!("{prefix}@.{suffix}"))
}

/// Switch a service from `old` to `new` units: link + reload + restart/start,
/// then stop and unlink units that disappeared.
pub fn switch(old: &Units, new: &Units) -> Result<()> {
    if old == new {
        return Ok(());
    }
    // Stop vanished units first: a renamed unit must release its ports and
    // sockets before its successor starts.
    for (unit, _) in old.iter().filter(|(u, _)| !new.contains_key(*u)) {
        // Stop before disable: disable unlinks the unit file.
        if !is_template(unit) && is_loadable(unit)? {
            systemctl(&["stop", unit])?;
            systemctl(&["disable", "--runtime", unit])?;
            let _ = Command::new("systemctl")
                .args(["reset-failed", unit])
                .output();
        }
        match fs::remove_file(Path::new(RUNTIME_UNIT_DIR).join(unit)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(Error::io(format!("unlink unit {unit}"))(e))
            }
            _ => {}
        }
    }
    for (unit, path) in new {
        link(unit, path)?;
    }
    systemctl(&["daemon-reload"])?;
    for (unit, path) in new {
        if old.get(unit) == Some(path) || is_template(unit) {
            continue;
        }
        let file = UnitFile::read(unit, path)?;
        if file.install {
            systemctl(&["enable", "--runtime", unit])?;
        }
        if !file.restart_if_changed {
            if file.install && !old.contains_key(unit) {
                systemctl(&["start", unit])?;
            }
        } else if file.install {
            systemctl(&["restart", unit])?;
        } else if old.contains_key(unit) {
            // No [Install]: the unit is pulled in on demand (socket- or
            // timer-activated). Restart it only if it is actually running;
            // starting it eagerly would e.g. fire a timer's job on deploy.
            systemctl(&["try-restart", unit])?;
        }
    }
    // Running instances of a changed template that flakelet did not
    // enumerate (socket-activated, generator-enabled).
    for (unit, path) in new {
        if !is_template(unit) || old.get(unit) == Some(path) || !old.contains_key(unit) {
            continue;
        }
        if !UnitFile::read(unit, path)?.restart_if_changed {
            continue;
        }
        for instance in running_instances(unit)? {
            if !new.contains_key(&instance) {
                systemctl(&["try-restart", &instance])?;
            }
        }
    }
    Ok(())
}

/// Stop a service and remove all its unit links.
pub fn remove(units: &Units) -> Result<()> {
    switch(units, &Units::new())
}

/// Keeps units linked. One job so sockets/timers cannot re-trigger the service.
pub fn stop_all(units: &Units) -> Result<()> {
    if units.is_empty() {
        return Ok(());
    }
    let mut args = vec!["stop"];
    args.extend(units.keys().filter(|u| !is_template(u)).map(String::as_str));
    systemctl(&args)
}

pub fn start_all(units: &Units) -> Result<()> {
    let mut args = vec!["start"];
    for (unit, path) in units {
        if !is_template(unit) && UnitFile::read(unit, path)?.install {
            args.push(unit);
        }
    }
    if args.len() == 1 {
        return Ok(());
    }
    systemctl(&args)
}

/// Re-link units at boot without starting anything; systemd targets pull them in.
pub fn relink(units: &Units) -> Result<()> {
    for (unit, path) in units {
        link(unit, path)?;
    }
    systemctl(&["daemon-reload"])?;
    for (unit, path) in units {
        if !is_template(unit) && UnitFile::read(unit, path)?.install {
            systemctl(&["enable", "--runtime", unit])?;
        }
    }
    Ok(())
}

fn is_loadable(unit: &str) -> Result<bool> {
    let out = Command::new("systemctl")
        .args(["show", "--property=LoadState", "--value", unit])
        .output()
        .map_err(|source| Error::Spawn {
            program: "systemctl".into(),
            source,
        })?;
    Ok(String::from_utf8_lossy(&out.stdout).trim() != "not-found")
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

/// Loaded, active instances of a template unit.
fn running_instances(template: &str) -> Result<Vec<String>> {
    let pattern = template.replacen("@.", "@*.", 1);
    let out = Command::new("systemctl")
        .args([
            "list-units",
            "--plain",
            "--no-legend",
            "--state=active",
            &pattern,
        ])
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
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let out = Command::new("systemctl")
        .args(["show", "--property=ActiveState", "--value"])
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
        .filter(|(_, s)| *s == "failed")
        .map(|(u, _)| u.clone())
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
