use crate::config::{Config, ServiceConfig};
use crate::error::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Runs nix commands, optionally as an unprivileged user with a shared cache.
pub struct Nix {
    eval_user: Option<String>,
    cache_dir: PathBuf,
}

impl Nix {
    pub fn new(cfg: &Config) -> Self {
        // Only drop privileges when we actually are root.
        let eval_user = if unsafe { libc::geteuid() } == 0 {
            cfg.eval_user.clone()
        } else {
            None
        };
        Self {
            eval_user,
            cache_dir: cfg.cache_dir.clone(),
        }
    }

    fn run(&self, program: &str, args: &[String]) -> Result<String> {
        let mut cmd = match &self.eval_user {
            Some(user) => {
                let mut c = Command::new("runuser");
                c.arg("-u").arg(user).arg("--").arg(program);
                c
            }
            None => Command::new(program),
        };
        let out = cmd
            .args(args)
            .env("XDG_CACHE_HOME", &self.cache_dir)
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
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub fn sha256_of_file(&self, path: &Path) -> Result<String> {
        self.run(
            "nix",
            &args(&["hash", "file", "--sri", &path.display().to_string()]),
        )
    }

    pub fn nar_hash(&self, store_path: &str) -> Result<String> {
        self.run("nix", &args(&["hash", "path", "--sri", store_path]))
    }

    pub fn add_to_store(&self, path: &Path) -> Result<String> {
        self.run(
            "nix",
            &args(&[
                "store",
                "add",
                "--mode",
                "flat",
                "--name",
                "flakelet-settings",
                &path.display().to_string(),
            ]),
        )
    }

    /// Locked flake URL (follows the ref, so this needs network for remote flakes).
    pub fn locked_url(&self, flake: &str) -> Result<LockedFlake> {
        let out = self.run(
            "nix",
            &args(&["flake", "metadata", "--refresh", "--json", flake]),
        )?;
        let meta: FlakeMetadata =
            serde_json::from_str(&out).map_err(Error::json("parse flake metadata"))?;
        Ok(LockedFlake {
            url: meta.url,
            rev: meta.revision.unwrap_or_default(),
        })
    }

    /// Evaluate the portable service function and return all image derivations.
    pub fn eval(&self, svc: &ServiceConfig, flake: &str, select: &str) -> Result<Vec<EvalJob>> {
        let mut a = vec![
            "--flake".into(),
            format!("{flake}#{}", svc.output),
            "--select".into(),
            select.into(),
        ];
        for (input, target) in &svc.input_overrides {
            a.extend(["--override-input".into(), input.clone(), target.clone()]);
        }
        let out = self.run("nix-eval-jobs", &a)?;
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
            if job.drv_path.is_some() {
                jobs.push(job);
            }
        }
        if jobs.is_empty() {
            return Err(Error::Deploy(format!(
                "flake output {flake}#{} produced no derivations",
                svc.output
            )));
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
        let out = self.run(
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
            .ok_or_else(|| Error::Deploy(format!("nix build {drv_path} produced no output")))
    }
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
    revision: Option<String>,
}

/// Build the --select expression: apply the flake's function to the host settings.
pub fn select_expr(
    settings_store_path: &str,
    settings_sha256: &str,
    nar_hashes: &BTreeMap<String, String>,
) -> String {
    let hashes = nar_hashes
        .iter()
        .map(|(p, h)| format!(r#""{p}" = "{h}";"#))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"f: {{ image = f rec {{
  settingsFile = builtins.fetchurl {{ url = "file://{path}"; sha256 = "{sha}"; }};
  settings = builtins.fromJSON (builtins.readFile settingsFile);
  storePath = p: (builtins.fetchTree {{ type = "path"; path = p; narHash = ({{ {hashes} }}).${{p}}; }}).outPath;
}}; }}"#,
        path = settings_store_path,
        sha = settings_sha256,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_expr_embeds_settings_and_hashes() {
        let mut hashes = BTreeMap::new();
        hashes.insert("/nix/store/aaa-dep".to_string(), "sha256-xyz".to_string());
        let expr = select_expr("/nix/store/bbb-settings", "sha256-abc", &hashes);
        assert!(expr.contains(r#"url = "file:///nix/store/bbb-settings""#));
        assert!(expr.contains(r#"sha256 = "sha256-abc""#));
        assert!(expr.contains(r#""/nix/store/aaa-dep" = "sha256-xyz";"#));
        assert!(expr.contains("storePath = p:"));
    }
}
