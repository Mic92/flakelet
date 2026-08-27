//! `flakelet export` / `import` archive format and the filesystem side of it.
//! Orchestration (locks, evaluation, activation) lives in `Manager`.

use crate::error::{Error, Result};
use crate::exports;
use crate::state::write_json_atomic;
use crate::svcstate::{Folder, StateInfo};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportMeta {
    pub version: u32,
    pub flakelet_version: String,
    pub name: String,
    pub source_host: String,
    pub created: u64,
    pub flake_url: String,
    pub flake_rev: String,
    pub settings_hash: String,
    pub state: StateInfo,
    pub exports: Value,
    /// How the folders were made consistent. Only "stopped" so far.
    pub consistency: String,
}

impl ExportMeta {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("meta.json");
        let data =
            fs::read_to_string(&path).map_err(Error::io(format!("read {}", path.display())))?;
        let meta: Self = serde_json::from_str(&data)
            .map_err(Error::json(format!("corrupt {}", path.display())))?;
        if meta.version > FORMAT_VERSION {
            return Err(Error::SchemaTooNew {
                path,
                found: meta.version,
                supported: FORMAT_VERSION,
            });
        }
        Ok(meta)
    }
}

/// Where the data of a folder really lives: DynamicUser= state sits under
/// /var/lib/private with /var/lib/<dir> being a symlink systemd maintains.
pub fn real_path(f: &Folder) -> PathBuf {
    if f.dynamic {
        if let Ok(rel) = f.path.strip_prefix("/var/lib") {
            return Path::new("/var/lib/private").join(rel);
        }
    }
    f.path.clone()
}

pub fn is_empty_dir(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut d) => d.next().is_none(),
        Err(_) => !path.exists(),
    }
}

/// Keeps the directory itself: for DynamicUser= it belongs to systemd.
pub fn clear_dir(path: &Path) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let _ = if e.file_type().is_ok_and(|t| t.is_dir()) {
            fs::remove_dir_all(&p)
        } else {
            fs::remove_file(&p)
        };
    }
}

pub fn tar_folder(folder: &Folder, out: &Path) -> Result<()> {
    let src = real_path(folder);
    if !src.is_dir() {
        // Never started: nothing to carry, but keep the member so indices line up.
        return run(
            "tar",
            &["-cf", &out.display().to_string(), "-T", "/dev/null"],
        );
    }
    run(
        "tar",
        &[
            "-C",
            &src.display().to_string(),
            "--numeric-owner",
            "--owner=0",
            "--group=0",
            "--xattrs",
            "-cf",
            &out.display().to_string(),
            ".",
        ],
    )
}

pub fn untar_folder(folder: &Folder, archive: &Path) -> Result<()> {
    let dst = real_path(folder);
    if folder.dynamic {
        let private = Path::new("/var/lib/private");
        fs::create_dir_all(private).map_err(Error::io("create /var/lib/private"))?;
        fs::set_permissions(private, fs::Permissions::from_mode(0o700))
            .map_err(Error::io("chmod /var/lib/private"))?;
    }
    fs::create_dir_all(&dst).map_err(Error::io(format!("create {}", dst.display())))?;
    run(
        "tar",
        &[
            "-C",
            &dst.display().to_string(),
            "--xattrs",
            "-xpf",
            &archive.display().to_string(),
        ],
    )?;
    if !folder.dynamic {
        run(
            "chown",
            &[
                "-R",
                &format!("{}:{}", folder.user, folder.group.as_deref().unwrap_or("")),
                &dst.display().to_string(),
            ],
        )?;
    }
    Ok(())
}

/// Inner folder tars stay uncompressed so only this layer compresses.
/// `-` streams to stdout / from stdin.
pub fn pack(dir: &Path, out: &Path) -> Result<()> {
    run_stdio(
        "tar",
        &[
            "-C",
            &dir.display().to_string(),
            "--zstd",
            "-cf",
            &out.display().to_string(),
            "meta.json",
            "service.json",
            "state",
            "requires",
        ],
    )
}

pub fn unpack(archive: &Path, dir: &Path) -> Result<()> {
    if archive.as_os_str() != "-" {
        fs::metadata(archive).map_err(Error::io(format!("open {}", archive.display())))?;
    }
    run_stdio(
        "tar",
        &[
            "-C",
            &dir.display().to_string(),
            "--zstd",
            "-xf",
            &archive.display().to_string(),
        ],
    )
}

/// Run each provider's dump (or restore) for the `requires.*` claims.
pub fn provider_hooks(
    exports: &Value,
    providers_dir: &Path,
    dir: &Path,
    restore: bool,
) -> Result<()> {
    let Some(requires) = exports.get("requires").and_then(Value::as_object) else {
        return Ok(());
    };
    let providers = exports::providers(providers_dir).unwrap_or_default();
    for (claim, body) in requires {
        let Some(hooks) = providers
            .iter()
            .find(|p| p.claim() == claim)
            .and_then(|p| p.state.as_ref())
        else {
            return Err(Error::NotTransferable {
                service: claim.clone(),
                verb: if restore { "restored" } else { "dumped" },
                reasons: vec![format!("no provider with state hooks for requires.{claim}")],
            });
        };
        let sub = dir.join("requires").join(claim);
        fs::create_dir_all(&sub).map_err(Error::io(format!("create {}", sub.display())))?;
        let claim_file = sub.join("claim.json");
        write_json_atomic(&claim_file, body)?;
        let hook = if restore { &hooks.restore } else { &hooks.dump };
        eprintln!("requires.{claim}: running {}", hook.display());
        run(
            &hook.display().to_string(),
            &[
                &claim_file.display().to_string(),
                &sub.display().to_string(),
            ],
        )?;
    }
    Ok(())
}

pub fn user_exists(name: &str) -> bool {
    Command::new("getent")
        .args(["passwd", name])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Path components equal to the old entry name become the new one,
/// mirroring how units derive their directories from `name`.
pub fn rename_folder(path: &Path, old: &str, new: &str) -> PathBuf {
    path.iter()
        .map(|c| if c == old { new.as_ref() } else { c })
        .collect()
}

pub fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}

/// Like `run` but with the caller's stdin/stdout, for streaming archives.
fn run_stdio(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|source| Error::Spawn {
            program: program.into(),
            source,
        })?;
    if !status.success() {
        return Err(Error::Command {
            program: program.into(),
            args: args.join(" "),
            stderr: String::new(),
        });
    }
    Ok(())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|source| Error::Spawn {
            program: program.into(),
            source,
        })?;
    if !out.status.success() {
        return Err(Error::Command {
            program: program.into(),
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
    fn folders_follow_the_entry_name() {
        assert_eq!(
            rename_folder(Path::new("/var/lib/web"), "web", "web2"),
            Path::new("/var/lib/web2")
        );
        assert_eq!(
            rename_folder(Path::new("/srv/webdata"), "web", "web2"),
            Path::new("/srv/webdata")
        );
    }

    #[test]
    fn dynamic_folders_live_in_private() {
        let f = Folder {
            path: "/var/lib/web".into(),
            dynamic: true,
            ..Default::default()
        };
        assert_eq!(real_path(&f), Path::new("/var/lib/private/web"));
        assert_eq!(
            real_path(&Folder {
                dynamic: false,
                ..f
            }),
            Path::new("/var/lib/web")
        );
    }

    #[test]
    fn folder_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("sub/f"), "data").unwrap();
        let me = rustix::process::getuid().as_raw().to_string();
        let folder = |p: &Path| Folder {
            path: p.into(),
            user: me.clone(),
            group: Some(rustix::process::getgid().as_raw().to_string()),
            dynamic: false,
        };
        let tar = tmp.path().join("0.tar");
        tar_folder(&folder(&src), &tar).unwrap();
        let dst = tmp.path().join("dst");
        untar_folder(&folder(&dst), &tar).unwrap();
        assert_eq!(fs::read_to_string(dst.join("sub/f")).unwrap(), "data");
        assert!(is_empty_dir(&tmp.path().join("nope")));
        assert!(!is_empty_dir(&dst));
    }
}
