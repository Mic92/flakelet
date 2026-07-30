use crate::error::{Error, Result};
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::Path;

/// A held flock; released on drop.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

fn flock(file: &File, op: libc::c_int) -> std::io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Acquire a lock. `exclusive`: LOCK_EX vs LOCK_SH. `wait`: block or fail immediately.
/// `info` is written into the lock file so waiters can see who holds it.
pub fn acquire(path: &Path, exclusive: bool, wait: bool, info: &str) -> Result<Lock> {
    let context = || format!("lock {}", path.display());
    if let Some(dir) = path.parent() {
        create_dir_all(dir).map_err(Error::io(context()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(Error::io(context()))?;

    let mut op = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    if !wait {
        op |= libc::LOCK_NB;
    }
    if let Err(e) = flock(&file, op) {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            let holder = std::fs::read_to_string(path).unwrap_or_default();
            return Err(Error::LockHeld {
                path: path.to_path_buf(),
                holder: holder.trim().into(),
            });
        }
        return Err(Error::Io {
            context: context(),
            source: e,
        });
    }
    if exclusive {
        let _ = file.set_len(0);
        let _ = writeln!(file, "pid {} {}", std::process::id(), info);
    }
    Ok(Lock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_locks_coexist_but_block_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc/lock");
        let _s1 = acquire(&path, false, false, "update a").unwrap();
        let _s2 = acquire(&path, false, false, "update b").unwrap();
        assert!(matches!(
            acquire(&path, true, false, "gc"),
            Err(Error::LockHeld { .. })
        ));
    }
}
