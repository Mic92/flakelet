use flakelet_core::config::ServiceConfig;
use flakelet_core::error::Error;
use flakelet_core::{CheckOpts, Manager, Result, UpdateOpts, UpdateOutcome};
use lexopt::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
flakelet - deploy systemd services from Nix flakes, evaluated at runtime

Usage: flakelet [--config <file>] <command> [options]

Commands:
  update [<name>...] [--force] [--no-wait] [--offline-fallback]
                        Evaluate, build and activate services (default: all)
  boot                  Re-link the current generations at boot, without evaluation
  deploy <name> --flake <ref> [--settings <file>] [--output <attr>] [update options]
                        Register and deploy a service outside the host configuration
  activate <name> <store path>
                        Register and start a prebuilt service artifact (no evaluation)
  remove <name>         Stop a service and delete its state and generations
  reconcile             Remove declarative services that vanished from the host configuration
  check [<name>...] [--build] [--gc-roots-dir <dir>]
                        Resolve and evaluate configured services without touching state (CI)
  driver [<name>...]    Print the rendered driver expression
  status [--json]       Show service status
  rollback <name>       Switch back to the previous generation
  lock <name>           Pin a service to the currently resolved flake revision
  unlock <name>         Remove the pin
  gc [--keep <n>]       Prune old generations

Options:
  --config <file>       Config file (default: /etc/flakelet/config.json)
  --build               Also build the evaluated artifacts (check)
  --gc-roots-dir <dir>  Root evaluated derivations and built artifacts there (check);
                        the directory must be reachable by the eval user
  --force               Retry even if the service is held after a failed deploy
  --no-wait             Fail instead of waiting for another flakelet operation
  --offline-fallback    Keep the current units when evaluation fails due to network errors
  --no-refresh          Do not bypass flake caches when resolving refs (offline use)
";

enum Cmd {
    Update {
        names: Vec<String>,
        opts: UpdateOpts,
    },
    Boot,
    Deploy {
        name: String,
        svc: Box<ServiceConfig>,
        opts: UpdateOpts,
    },
    Remove {
        name: String,
    },
    Reconcile,
    Check {
        names: Vec<String>,
        check: CheckOpts,
    },
    Driver {
        names: Vec<String>,
        opts: UpdateOpts,
    },
    Status {
        json: bool,
    },
    Rollback {
        name: String,
    },
    Lock {
        name: String,
    },
    Unlock {
        name: String,
    },
    Gc {
        keep: Option<u32>,
    },
}

struct Cli {
    config: PathBuf,
    command: Cmd,
}

fn main() -> ExitCode {
    let cli = match parse_args() {
        Ok(Some(cli)) => cli,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("error: {err}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match run(&cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Ok(None) means: help requested.
fn parse_args() -> std::result::Result<Option<Cli>, lexopt::Error> {
    let mut parser = lexopt::Parser::from_env();
    let mut config = PathBuf::from("/etc/flakelet/config.json");

    // Global options until the first positional argument (the command).
    let command = loop {
        match parser.next()? {
            Some(Long("config")) => config = parser.value()?.into(),
            Some(Long("help")) | Some(Short('h')) => return Ok(None),
            Some(Value(v)) => break v.string()?,
            Some(arg) => return Err(arg.unexpected()),
            None => return Err("missing command".into()),
        }
    };

    let mut names = Vec::new();
    let mut opts = UpdateOpts::default();
    let mut flake = None;
    let mut settings = None;
    let mut output = None;
    let mut json = false;
    let mut keep = None;
    let mut check = CheckOpts::default();
    while let Some(arg) = parser.next()? {
        match arg {
            Long("force") => opts.force = true,
            Long("no-wait") => opts.no_wait = true,
            Long("offline-fallback") => opts.offline_fallback = true,
            Long("no-refresh") => opts.no_refresh = true,
            Long("flake") => flake = Some(parser.value()?.string()?),
            Long("settings") => settings = Some(PathBuf::from(parser.value()?)),
            Long("output") => output = Some(parser.value()?.string()?),
            Long("json") => json = true,
            Long("build") => check.build = true,
            Long("gc-roots-dir") => check.gc_roots_dir = Some(PathBuf::from(parser.value()?)),
            Long("keep") => keep = Some(parser.value()?.parse()?),
            Long("help") | Short('h') => return Ok(None),
            Value(v) => names.push(v.string()?),
            _ => return Err(arg.unexpected()),
        }
    }

    let one_name = |names: &[String]| -> std::result::Result<String, lexopt::Error> {
        match names {
            [name] => Ok(name.clone()),
            _ => Err("expected exactly one service name".into()),
        }
    };
    let command = match command.as_str() {
        "update" => Cmd::Update { names, opts },
        "boot" => Cmd::Boot,
        "deploy" => Cmd::Deploy {
            name: one_name(&names)?,
            svc: Box::new(ServiceConfig {
                flake: flake.ok_or("deploy requires --flake")?,
                settings: read_settings(settings.as_deref()).map_err(|e| e.to_string())?,
                output: output.unwrap_or_else(|| ServiceConfig::default().output),
                ..Default::default()
            }),
            opts,
        },
        "activate" => match &names[..] {
            [name, path] => Cmd::Deploy {
                name: name.clone(),
                svc: Box::new(ServiceConfig {
                    prebuilt: Some(path.into()),
                    ..Default::default()
                }),
                opts,
            },
            _ => return Err("activate expects <name> <store path>".into()),
        },
        "remove" => Cmd::Remove {
            name: one_name(&names)?,
        },
        "reconcile" => Cmd::Reconcile,
        "check" => {
            check.refresh = !opts.no_refresh;
            Cmd::Check { names, check }
        }
        "driver" => Cmd::Driver { names, opts },
        "status" => Cmd::Status { json },
        "rollback" => Cmd::Rollback {
            name: one_name(&names)?,
        },
        "lock" => Cmd::Lock {
            name: one_name(&names)?,
        },
        "unlock" => Cmd::Unlock {
            name: one_name(&names)?,
        },
        "gc" => Cmd::Gc { keep },
        other => return Err(format!("unknown command '{other}'").into()),
    };
    Ok(Some(Cli { config, command }))
}

fn run(cli: &Cli) -> Result<bool> {
    let mgr = Manager::load(&cli.config)?;
    match &cli.command {
        Cmd::Update { names, opts } => {
            let names = if names.is_empty() {
                for removed in mgr.reconcile()? {
                    println!("{removed}: removed (no longer configured)");
                }
                mgr.services()?.into_keys().collect()
            } else {
                names.clone()
            };
            return Ok(update_all(&mgr, &names, *opts));
        }
        Cmd::Boot => {
            for name in mgr.boot()? {
                println!("{name}: units re-linked");
            }
        }
        Cmd::Deploy { name, svc, opts } => {
            let outcome = mgr.deploy(name, svc, *opts)?;
            println!("{name}: {}", describe(&outcome));
            return Ok(!matches!(outcome, UpdateOutcome::RolledBack { .. }));
        }
        Cmd::Remove { name } => {
            mgr.remove(name)?;
            println!("{name}: removed");
        }
        Cmd::Reconcile => {
            for removed in mgr.reconcile()? {
                println!("{removed}: removed (no longer configured)");
            }
        }
        Cmd::Check { names, check } => {
            for result in mgr.check(names, check)? {
                match &result.out {
                    Some(out) => println!("{}: built {}", result.name, out.display()),
                    None => println!("{}: evaluated {}", result.name, result.drv_path),
                }
            }
        }
        Cmd::Driver { names, opts } => {
            print!("{}", mgr.render_driver(names, !opts.no_refresh)?);
        }
        Cmd::Status { json } => print_status(&mgr, *json)?,
        Cmd::Rollback { name } => {
            let generation = mgr.rollback(name)?;
            println!("{name}: rolled back to generation {generation}");
        }
        Cmd::Lock { name } => {
            let url = mgr.lock_service(name)?;
            println!("{name}: pinned to {url}");
        }
        Cmd::Unlock { name } => {
            mgr.unlock_service(name)?;
            println!("{name}: unpinned");
        }
        Cmd::Gc { keep } => mgr.gc(*keep)?,
    }
    Ok(true)
}

fn read_settings(path: Option<&Path>) -> Result<serde_json::Value> {
    let Some(path) = path else {
        return Ok(serde_json::json!({}));
    };
    let data =
        fs::read_to_string(path).map_err(Error::io(format!("read settings {}", path.display())))?;
    serde_json::from_str(&data).map_err(Error::json(format!("parse settings {}", path.display())))
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
