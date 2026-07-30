use crate::error::{Error, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

fn run(args: &[String]) -> Result<String> {
    let out = Command::new("portablectl")
        .args(args)
        .output()
        .map_err(|source| Error::Spawn {
            program: "portablectl".into(),
            source,
        })?;
    if !out.status.success() {
        return Err(Error::Command {
            program: "portablectl".into(),
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[derive(Debug, Deserialize)]
struct AttachedImage {
    name: String,
}

/// Names of currently attached portable images.
pub fn list() -> Result<Vec<String>> {
    let out = run(&["list".into(), "--json=short".into()])?;
    if out.is_empty() {
        return Ok(Vec::new());
    }
    let images: Vec<AttachedImage> =
        serde_json::from_str(&out).map_err(Error::json("parse portablectl list output"))?;
    Ok(images.into_iter().map(|i| i.name).collect())
}

/// Portable service name of an image file: basename without version suffix and .raw.
pub fn image_name(image: &Path) -> String {
    let base = image
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    base.split('_').next().unwrap_or(base).to_string()
}

pub fn attach(image: &Path, profile: &str, extra: &[String], reattach: bool) -> Result<()> {
    let verb = if reattach { "reattach" } else { "attach" };
    let mut args = vec![
        verb.into(),
        "--now".into(),
        "--enable".into(),
        format!("--profile={profile}"),
    ];
    args.extend(extra.iter().cloned());
    args.push(image.display().to_string());
    run(&args).map(|_| ())
}

pub fn detach(image: &Path) -> Result<()> {
    run(&["detach".into(), "--now".into(), image.display().to_string()]).map(|_| ())
}
