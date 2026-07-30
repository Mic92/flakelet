use clap::{Args, Parser, Subcommand};
use flakelet_core::config::ServiceConfig;
use flakelet_core::{Manager, Result, UpdateOpts, UpdateOutcome};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "flakelet",
    about = "Deploy systemd portable services from Nix flakes"
)]
struct Cli {
    #[arg(long, default_value = "/etc/flakelet/config.json")]
    config: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Args, Clone, Copy, Default)]
struct UpdateFlags {
    /// Retry even if the service is held after a failed deploy.
    #[arg(long)]
    force: bool,
    /// Fail instead of waiting when another flakelet operation holds the lock.
    #[arg(long)]
    no_wait: bool,
    /// Keep the current attachment when evaluation fails due to network errors.
    #[arg(long)]
    offline_fallback: bool,
}

impl From<UpdateFlags> for UpdateOpts {
    fn from(f: UpdateFlags) -> Self {
        Self {
            force: f.force,
            no_wait: f.no_wait,
            offline_fallback: f.offline_fallback,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Evaluate, build and (re)attach services (all configured ones if no name is given).
    Update {
        names: Vec<String>,
        #[command(flatten)]
        flags: UpdateFlags,
    },
    /// Register and deploy a service that is not part of the host configuration.
    Deploy {
        name: String,
        #[arg(long)]
        flake: String,
        #[arg(long)]
        settings: Option<PathBuf>,
        #[arg(long)]
        output: Option<String>,
        #[arg(long, default_value = "default")]
        profile: String,
        #[command(flatten)]
        flags: UpdateFlags,
    },
    /// Detach a service and delete its state and generations.
    Remove { name: String },
    /// Remove declarative services that vanished from the host configuration.
    Reconcile,
    /// Show service status.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Reattach the previous generation.
    Rollback { name: String },
    /// Pin a service to the currently resolved flake revision.
    Lock { name: String },
    /// Remove the pin.
    Unlock { name: String },
    /// Prune old generations.
    Gc {
        #[arg(long)]
        keep: Option<u32>,
    },
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(ok) => {
            if ok {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::FAILURE
            }
        }
        Err(err) => {
            eprintln!("error: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<bool> {
    let cli = Cli::parse();
    let mgr = Manager::load(&cli.config)?;
    match cli.command {
        Cmd::Update { names, flags } => {
            let names = if names.is_empty() {
                for removed in mgr.reconcile()? {
                    println!("{removed}: removed (no longer configured)");
                }
                mgr.services()?.into_keys().collect()
            } else {
                names
            };
            return Ok(update_all(&mgr, &names, flags.into()));
        }
        Cmd::Deploy {
            name,
            flake,
            settings,
            output,
            profile,
            flags,
        } => {
            let svc = ServiceConfig {
                flake,
                settings_file: settings,
                output: output.unwrap_or_else(|| ServiceConfig::default().output),
                profile,
                ..Default::default()
            };
            let outcome = mgr.deploy(&name, &svc, flags.into())?;
            println!("{name}: {}", describe(&outcome));
            return Ok(!matches!(outcome, UpdateOutcome::RolledBack { .. }));
        }
        Cmd::Remove { name } => {
            mgr.remove(&name)?;
            println!("{name}: removed");
        }
        Cmd::Reconcile => {
            for removed in mgr.reconcile()? {
                println!("{removed}: removed (no longer configured)");
            }
        }
        Cmd::Status { json } => print_status(&mgr, json)?,
        Cmd::Rollback { name } => {
            let generation = mgr.rollback(&name)?;
            println!("{name}: rolled back to generation {generation}");
        }
        Cmd::Lock { name } => {
            let url = mgr.lock_service(&name)?;
            println!("{name}: pinned to {url}");
        }
        Cmd::Unlock { name } => {
            mgr.unlock_service(&name)?;
            println!("{name}: unpinned");
        }
        Cmd::Gc { keep } => mgr.gc(keep)?,
    }
    Ok(true)
}

fn update_all(mgr: &Manager, names: &[String], opts: UpdateOpts) -> bool {
    let mut ok = true;
    for name in names {
        match mgr.update(name, opts) {
            Ok(outcome) => {
                println!("{name}: {}", describe(&outcome));
                ok &= !matches!(outcome, UpdateOutcome::RolledBack { .. });
            }
            Err(err) => {
                eprintln!("{name}: error: {err}");
                ok = false;
            }
        }
    }
    ok
}

fn print_status(mgr: &Manager, json: bool) -> Result<()> {
    let status = mgr.status()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status).expect("status is serializable")
        );
        return Ok(());
    }
    for s in status {
        let state = if s.held.is_some() {
            "held"
        } else if s.degraded {
            "degraded"
        } else if s.updating {
            "updating"
        } else if s.generation.is_some() {
            "ok"
        } else {
            "not deployed"
        };
        println!(
            "{}\t{}\tgen {}\t{}",
            s.name,
            state,
            s.generation.map_or("-".into(), |g| g.to_string()),
            s.flake
        );
    }
    Ok(())
}

fn describe(outcome: &UpdateOutcome) -> String {
    match outcome {
        UpdateOutcome::UpToDate => "up to date".into(),
        UpdateOutcome::Updated { generation } => format!("updated to generation {generation}"),
        UpdateOutcome::Degraded { reason } => {
            format!("degraded, kept current generation: {reason}")
        }
        UpdateOutcome::Held { reason } => format!("held after previous failure: {reason}"),
        UpdateOutcome::RolledBack { reason } => format!("deploy failed, rolled back: {reason}"),
    }
}
