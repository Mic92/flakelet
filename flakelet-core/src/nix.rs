use crate::config::{Config, Credentials, EvalSettings};
use crate::error::{Error, Result};
use serde::Deserialize;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Runs nix commands, optionally as an unprivileged user with a shared cache
/// and fetch credentials for private flakes.
pub struct Nix {
    /// uid/gid to drop to for evaluation and fetching (owner of the cache dir).
    eval_user: Option<(u32, u32)>,
    cache_dir: PathBuf,
    eval: EvalSettings,
    credentials: Credentials,
}

impl Nix {
    pub fn new(cfg: &Config, credentials: Option<&Credentials>) -> Self {
        // Only drop privileges when we actually are root. The eval user's
        // uid/gid is taken from the cache dir it owns; no NSS lookup needed.
        let eval_user = (rustix::process::geteuid().is_root() && cfg.eval_user.is_some())
            .then(|| rustix::fs::stat(&cfg.cache_dir).ok())
            .flatten()
            .map(|st| (st.st_uid, st.st_gid))
            .filter(|&(uid, _)| uid != 0);
        Self {
            eval_user,
            cache_dir: cfg.cache_dir.clone(),
            eval: cfg.eval.clone(),
            credentials: credentials
                .or(cfg.credentials.as_ref())
                .cloned()
                .unwrap_or_default(),
        }
    }

    /// Run as the unprivileged eval user (evaluation and fetching).
    fn run(&self, program: &str, args: &[String]) -> Result<String> {
        self.run_as(self.eval_user, program, args)
    }

    /// Run as the calling user (store writes and builds go through the daemon).
    fn run_root(&self, program: &str, args: &[String]) -> Result<String> {
        self.run_as(None, program, args)
    }

    fn run_as(&self, user: Option<(u32, u32)>, program: &str, args: &[String]) -> Result<String> {
        let mut cmd = Command::new(program);
        if let Some((uid, gid)) = user {
            cmd.uid(uid).gid(gid);
        }
        // HOME must be readable by the eval user (~/.nix-defexpr etc.); the
        // cache dir is owned by it.
        cmd.args(args)
            .env("HOME", &self.cache_dir)
            .env("XDG_CACHE_HOME", &self.cache_dir);
        self.apply_credentials(&mut cmd)?;
        let out = cmd.output().map_err(|source| Error::Spawn {
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
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn apply_credentials(&self, cmd: &mut Command) -> Result<()> {
        let creds = &self.credentials;
        let mut nix_config = String::new();
        if let Some(netrc) = &creds.netrc_file {
            nix_config.push_str(&format!("netrc-file = {}\n", netrc.display()));
        }
        if let Some(tokens) = &creds.access_tokens_file {
            // Tokens go through the environment, never onto the command line.
            let data = fs::read_to_string(tokens)
                .map_err(Error::io(format!("read {}", tokens.display())))?;
            let tokens = data.split_whitespace().collect::<Vec<_>>().join(" ");
            nix_config.push_str(&format!("access-tokens = {tokens}\n"));
        }
        if !nix_config.is_empty() {
            cmd.env("NIX_CONFIG", nix_config);
        }
        if let Some(key) = &creds.ssh_key_file {
            let mut ssh = format!(
                "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes",
                key.display()
            );
            if let Some(known_hosts) = &creds.ssh_known_hosts_file {
                ssh.push_str(&format!(" -o UserKnownHostsFile={}", known_hosts.display()));
            }
            cmd.env("GIT_SSH_COMMAND", ssh);
        }
        Ok(())
    }

    /// Locked flake URL including narHash, suitable for a pure builtins.getFlake.
    /// `refresh` bypasses the tarball/eval caches to see new upstream revisions.
    pub fn locked_url(&self, flake: &str, refresh: bool) -> Result<LockedFlake> {
        let mut a = args(&["flake", "metadata", "--json"]);
        if refresh {
            a.push("--refresh".into());
        }
        a.push(flake.into());
        let out = self.run("nix", &a)?;
        let meta: FlakeMetadata =
            serde_json::from_str(&out).map_err(Error::json("parse flake metadata"))?;
        let sep = if meta.url.contains('?') { '&' } else { '?' };
        let nar_hash = meta.locked.nar_hash.replace('+', "%2B").replace('=', "%3D");
        Ok(LockedFlake {
            url: format!("{}{sep}narHash={nar_hash}", meta.url),
            rev: meta.locked.rev.unwrap_or(meta.locked.nar_hash),
        })
    }

    /// Add a rendered driver expression to the store.
    pub fn add_driver(&self, driver_file: &Path) -> Result<PathBuf> {
        self.run_root(
            "nix",
            &args(&[
                "store",
                "add",
                "--mode",
                "flat",
                "--name",
                "flakelet-driver.nix",
                &driver_file.display().to_string(),
            ]),
        )
        .map(PathBuf::from)
    }

    /// Evaluate a driver expression with nix-eval-jobs.
    pub fn eval_driver(&self, driver: &Path) -> Result<Vec<EvalJob>> {
        let workers = self.eval.workers.unwrap_or(1);
        let max_memory = self
            .eval
            .max_memory_mb
            .unwrap_or_else(default_max_memory_mb);
        let out = self.run(
            "nix-eval-jobs",
            &args(&[
                "--workers",
                &workers.to_string(),
                "--max-memory-size",
                &max_memory.to_string(),
                &driver.display().to_string(),
            ]),
        )?;
        let mut jobs = Vec::new();
        for line in out.lines().filter(|l| !l.trim().is_empty()) {
            let job: EvalJob =
                serde_json::from_str(line).map_err(Error::json("parse nix-eval-jobs output"))?;
            if let Some(message) = job.error {
                return Err(Error::Eval {
                    attr: job.attr,
                    message,
                });
            }
            jobs.push(job);
        }
        Ok(jobs)
    }

    /// Store paths of the flake source and all its inputs (fetches them if needed),
    /// so they can be gc-rooted for offline re-evaluation.
    pub fn flake_source_paths(&self, flake: &str) -> Result<Vec<String>> {
        let out = self.run("nix", &args(&["flake", "archive", "--json", flake]))?;
        let tree: serde_json::Value =
            serde_json::from_str(&out).map_err(Error::json("parse nix flake archive output"))?;
        let mut paths = Vec::new();
        collect_archive_paths(&tree, &mut paths);
        Ok(paths)
    }

    /// Build a derivation with an out-link (indirect gc root) and return its output path.
    pub fn build(&self, drv_path: &str, out_link: &Path) -> Result<PathBuf> {
        let out = self.run_root(
            "nix",
            &args(&[
                "build",
                &format!("{drv_path}^*"),
                "--print-out-paths",
                "--out-link",
                &out_link.display().to_string(),
            ]),
        )?;
        out.lines()
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| Error::NoBuildOutput(drv_path.into()))
    }
}

/// Half of MemAvailable, capped at 4 GiB, so eval cannot starve the machine.
fn default_max_memory_mb() -> u64 {
    let available_kb = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemAvailable:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(4 * 1024 * 1024);
    (available_kb / 2 / 1024).clamp(512, 4096)
}

/// `nix flake archive --json` output: nested { path, inputs: { <name>: ... } }.
fn collect_archive_paths(value: &serde_json::Value, out: &mut Vec<String>) {
    let Some(obj) = value.as_object() else { return };
    if let Some(path) = obj.get("path").and_then(|p| p.as_str()) {
        out.push(path.to_string());
    }
    for input in obj
        .get("inputs")
        .and_then(|i| i.as_object())
        .into_iter()
        .flatten()
    {
        collect_archive_paths(input.1, out);
    }
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

pub struct LockedFlake {
    pub url: String,
    pub rev: String,
}

#[derive(Debug, Deserialize)]
pub struct EvalJob {
    pub attr: String,
    #[serde(rename = "drvPath")]
    pub drv_path: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct FlakeMetadata {
    url: String,
    locked: LockedNode,
}

#[derive(Deserialize)]
struct LockedNode {
    #[serde(rename = "narHash")]
    nar_hash: String,
    rev: Option<String>,
}
