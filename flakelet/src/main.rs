use flakelet_core::config::ServiceConfig;
use flakelet_core::error::Error;
use flakelet_core::nix::Nix;
use flakelet_core::{CheckOpts, Manager, Result, UpdateOpts, UpdateOutcome};
use lexopt::prelude::*;
use serde_json::Value;
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
  check [<name>...] [--build] [--gc-roots-dir <dir>] [--machine <name> [--flake <ref>]]
                        Resolve and evaluate configured services without touching state (CI)
  build <name>... [--out-link <dir>] [--machine <name> [--flake <ref>]]
                        Like check, but build the artifacts with out-links in <dir> (default: .)
  driver [<name>...] [--machine <name> [--flake <ref>]]
                        Print the rendered driver expression
  status [<name>...] [--json]
                        Show service status
  diff <name>           Closure diff between the running generation and a fresh evaluation
  rollback <name>       Switch back to the previous generation
  lock <name>           Pin a service to the currently resolved flake revision
  unlock <name>         Remove the pin
  gc [--keep <n>]       Prune old generations

Options:
  --config <file>       Config file (default: /etc/flakelet/config.json)
  --machine <name>      Use the flakelet config of nixosConfigurations.<name> from --flake
                        (default: the flake in the current directory) instead of --config
  --build               Also build the evaluated artifacts (check)
  --out-link <dir>      Directory for per-service result symlinks (build)
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
        names: Vec<String>,
    },
    Diff {
        name: String,
        refresh: bool,
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

/// Where the flakelet configuration comes from.
enum ConfigSource {
    File(PathBuf),
    /// A nixosConfiguration in a flake (`--machine`, off-machine check).
    Machine {
        flake: String,
        name: String,
    },
}

struct Cli {
    config: ConfigSource,
    config_explicit: bool,
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
            eprintln!("error: {err}\ntry 'flakelet --help'");
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
    let mut config: Option<PathBuf> = None;

    // Global options until the first positional argument (the command).
    let command = loop {
        match parser.next()? {
            Some(Long("config")) => config = Some(parser.value()?.into()),
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
    let mut machine = None;
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
            Long("out-link") => check.out_links = Some(PathBuf::from(parser.value()?)),
            Long("machine") => machine = Some(parser.value()?.string()?),
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
                flake: flake.clone().ok_or("deploy requires --flake")?,
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
        "build" => {
            if names.is_empty() {
                return Err("build expects at least one service name".into());
            }
            check.build = true;
            check.refresh = !opts.no_refresh;
            check.out_links.get_or_insert_with(|| ".".into());
            Cmd::Check { names, check }
        }
        "driver" => Cmd::Driver { names, opts },
        "status" => Cmd::Status { json, names },
        "diff" => Cmd::Diff {
            name: one_name(&names)?,
            refresh: !opts.no_refresh,
        },
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
    let config_explicit = config.is_some();
    let config = match (machine, config) {
        (None, config) => {
            ConfigSource::File(config.unwrap_or_else(|| "/etc/flakelet/config.json".into()))
        }
        (Some(_), Some(_)) => return Err("--machine and --config are mutually exclusive".into()),
        (Some(name), None) if matches!(command, Cmd::Check { .. } | Cmd::Driver { .. }) => {
            ConfigSource::Machine {
                flake: flake.unwrap_or_else(|| ".".into()),
                name,
            }
        }
        (Some(_), None) => return Err("--machine is only supported for check and driver".into()),
    };
    Ok(Some(Cli {
        config,
        config_explicit,
        command,
    }))
}

fn run(cli: &Cli) -> Result<bool> {
    let config = match &cli.config {
        ConfigSource::File(path) => {
            if cli.config_explicit && !path.exists() {
                return Err(Error::io(format!("cannot read config {}", path.display()))(
                    std::io::ErrorKind::NotFound.into(),
                ));
            }
            path.clone()
        }
        // Off-machine: build the machine's rendered config.json from its flake.
        ConfigSource::Machine { flake, name } => Nix::new(&flakelet_core::Config::default(), None)
            .build_attr(&format!(
                "{flake}#nixosConfigurations.{name}.config.services.flakelets.configFile"
            ))?,
    };
    let mgr = Manager::load(&config)?;
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
                for claim in &result.missing_providers {
                    eprintln!("{}: warning: no provider announces '{claim}'", result.name);
                }
            }
        }
        Cmd::Driver { names, opts } => {
            print!("{}", mgr.render_driver(names, !opts.no_refresh)?);
        }
        Cmd::Status { json, names } => print_status(&mgr, *json, names)?,
        Cmd::Diff { name, refresh } => {
            let diff = mgr.diff(name, *refresh)?;
            if diff.is_empty() {
                println!("{name}: no closure changes");
            } else {
                println!("{diff}");
            }
        }
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
        Cmd::Gc { keep } => {
            let pruned = mgr.gc(*keep)?;
            if pruned.is_empty() {
                println!("nothing to prune");
            }
            for (name, gens) in pruned {
                let gens: Vec<String> = gens.iter().map(u32::to_string).collect();
                println!("{name}: pruned generation(s) {}", gens.join(", "));
            }
        }
    }
    Ok(true)
}

fn read_settings(path: Option<&Path>) -> Result<Value> {
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

fn print_status(mgr: &Manager, json: bool, names: &[String]) -> Result<()> {
    let mut status = mgr.status()?;
    if !names.is_empty() {
        for name in names {
            if !status.iter().any(|s| &s.name == name) {
                return Err(Error::UnknownService(name.clone()));
            }
        }
        status.retain(|s| names.contains(&s.name));
    }
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
        let pin = if s.pin.is_some() { "\t(pinned)" } else { "" };
        println!(
            "{}\t{}\tgen {}\t{}{}",
            s.name,
            state,
            s.generation.map_or("-".into(), |g| g.to_string()),
            s.flake,
            pin
        );
        if let Some(err) = s.held.as_deref().or(s.last_error.as_deref()) {
            let mut lines = err.lines().map(str::trim);
            let line = lines.clone().rfind(|l| l.contains("error:"));
            let line = line.or_else(|| lines.next()).unwrap_or(err);
            println!("\tlast error: {line}");
        }
        for claim in &s.missing_providers {
            eprintln!("{}: warning: no provider announces '{claim}'", s.name);
        }
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
