use flakelet_core::config::ServiceConfig;
use flakelet_core::error::Error;
use flakelet_core::nix::Nix;
use flakelet_core::state::{DisabledBy, Origin};
use flakelet_core::{CheckOpts, Manager, Result, UpdateOpts, UpdateOutcome};
use lexopt::prelude::*;
use serde_json::Value;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
flakelet - deploy systemd services from Nix flakes, evaluated at runtime

Usage: flakelet [--config <file>] <command> [options]

Commands:
  update [<name>...] [--force] [--no-wait] [--offline-fallback] [--flake <ref>]
                        Evaluate, build and activate services (default: all)
  boot                  Re-link the current generations at boot, without evaluation
  deploy <name> --flake <ref> [--settings <file>] [--output <attr>] [update options]
                        Register and deploy a service outside the host configuration
  activate <name> <store path>
                        Register and start a prebuilt service artifact (no evaluation)
  remove <name>         Stop a service and delete its generations
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
  disable <name> [-m <reason>]
                        Stop the service and keep it stopped across updates and reboots
  enable <name>         Start a disabled service again on its current generation
  export <name> [-o <file>|-] [--to <host>] [--copy] [--dry-run]
                        Stop the service, archive its state and leave it disabled
  import <file>|- [--name <name>] [--settings <file>] [--replace] [update options]
                        Restore an exported service and start it
  lock <name>           Pin a service to the currently deployed flake revision
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

Examples:
  flakelet update                     Update all services, remove vanished ones
  flakelet deploy web --flake github:me/web
  flakelet status --json
  flakelet check --machine web01 --flake .

Run 'flakelet <command> --help' for command-specific examples.
";

fn command_help(command: &str) -> Option<&'static str> {
    Some(match command {
        "update" => "\
Usage: flakelet update [<name>...] [--force] [--no-wait] [--offline-fallback] [--no-refresh] [--flake <ref>]

Evaluate, build and activate services. Without names all configured
services are updated and vanished declarative services are removed.

--flake deploys one service from another ref for testing. The next
update without it (including host activations) reverts to the
configured ref.

Examples:
  flakelet update
  flakelet update web api
  flakelet update web --force            Retry a held service
  flakelet update web --flake github:me/web/fix-branch
  flakelet update --offline-fallback     Keep running units when the network is down
",
        "boot" => "\
Usage: flakelet boot

Re-link the current generations at boot, without evaluation.
Run by the flakelet-boot service; rarely needed manually.
",
        "deploy" => "\
Usage: flakelet deploy <name> --flake <ref> [--settings <file>] [--output <attr>]

Register and deploy a service outside the host configuration.

Examples:
  flakelet deploy web --flake github:me/web
  flakelet deploy web --flake github:me/web/v1.2 --settings prod.json
  flakelet deploy web --flake . --output flakelet.web
",
        "activate" => "\
Usage: flakelet activate <name> <store path>

Register and start a prebuilt service artifact, no evaluation. Build it
first with 'flakelet build' and copy the closure to the target host.

Example:
  flakelet activate web /nix/store/...-flakelet-web
",
        "remove" => "\
Usage: flakelet remove [--purge] <name>

Stop a service, unlink its units and delete its generations and flakelet
bookkeeping. The service's own state folders (StateDirectory=) are kept
and listed. --purge empties them too.

Example:
  flakelet remove web
",
        "reconcile" => "\
Usage: flakelet reconcile

Remove declarative services that vanished from the host configuration.
Also runs as part of 'flakelet update' without names.
",
        "check" => "\
Usage: flakelet check [<name>...] [--build] [--gc-roots-dir <dir>] [--machine <name> [--flake <ref>]]

Resolve and evaluate configured services without touching state (CI).

Examples:
  flakelet check
  flakelet check web --build
  flakelet check --machine web01 --flake github:me/infra
",
        "build" => "\
Usage: flakelet build <name>... [--out-link <dir>] [--machine <name> [--flake <ref>]]

Like check, but build the artifacts with out-links in <dir> (default: .).

Examples:
  flakelet build web
  flakelet build web --out-link /tmp/artifacts --machine web01
",
        "driver" => "\
Usage: flakelet driver [<name>...] [--machine <name> [--flake <ref>]]

Print the rendered driver expression, the Nix code flakelet evaluates.

Example:
  flakelet driver web
",
        "status" => "\
Usage: flakelet status [<name>...] [--json]

Show service status. Names filter the output.

Examples:
  flakelet status
  flakelet status web --json
",
        "diff" => "\
Usage: flakelet diff <name> [--no-refresh]

Closure diff between the running generation and a fresh evaluation.

Example:
  flakelet diff web
",
        "disable" => "\
Usage: flakelet disable <name> [-m <reason>]

Stop and unlink the service's units and mark it disabled. Updates, host
activations and reboots leave it alone until 'flakelet enable'.
Generations and state folders are kept.

Example:
  flakelet disable web -m 'db maintenance'
",
        "enable" => "\
Usage: flakelet enable <name>

Clear the disabled mark and start the current generation again. Nothing
is evaluated, so this works offline. Run 'flakelet update' afterwards to
catch up.
",
        "export" => "\
Usage: flakelet export <name> [-o <file>|-] [--to <host>] [--copy] [--dry-run]

Stop all units of the service, run its <name>-dump.service and provider
dumps and archive the StateDirectory= folders. The service is then left
disabled on this host so it does not run next to the imported copy;
'flakelet enable' aborts the move. --to only labels the reason shown in
status. --copy starts the units again instead (backups, clones).
The zstd tar goes to stdout unless -o names a file. It carries the locked
flake ref and the state, but no settings, store paths or secrets.
--dry-run prints what would be exported as JSON, or why not.

Examples:
  flakelet export web --to hostb | ssh hostb flakelet import -
  flakelet export web --copy -o web.flakelet.tar.zst
  flakelet export web --dry-run | jq .state
",
        "import" => "\
Usage: flakelet import <file>|- [--name <name>] [--settings <file>] [update options]

Build the exported service (pinned to the exported revision), restore its
state folders and provider resources, run <name>-restore.service and
activate. If <name> is already declared on this host that entry is used
and --settings is ignored. Otherwise a manual service is registered with
the settings from --settings (default: none). State folders on this host
must be empty; --replace clears them (and sets FLAKELET_REPLACE=1 for
provider restore hooks) when this host already ran the service.

Examples:
  ssh hosta flakelet export web | flakelet import -
  flakelet import web.flakelet.tar.zst --name web2 --settings web2.json
",
        "rollback" => "\
Usage: flakelet rollback <name>

Switch back to the previous generation. The next update rolls forward
again; use 'flakelet lock' to stay on the current revision.

Example:
  flakelet rollback web
",
        "lock" => "\
Usage: flakelet lock <name>

Pin a service to the revision of its active generation. Updates keep
deploying the pinned revision until 'flakelet unlock'.

Example:
  flakelet lock web
",
        "unlock" => "\
Usage: flakelet unlock <name>

Remove the pin set by 'flakelet lock'.
",
        "gc" => "\
Usage: flakelet gc [--keep <n>]

Prune old generations. --keep overrides the per-service setting.

Examples:
  flakelet gc
  flakelet gc --keep 1
",
        _ => return None,
    })
}

enum Cmd {
    Update {
        names: Vec<String>,
        opts: UpdateOpts,
    },
    Boot,
    Deploy {
        name: String,
        svc: Box<ServiceConfig>,
        settings: Option<PathBuf>,
        opts: UpdateOpts,
    },
    Remove {
        name: String,
        purge: bool,
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
    Disable {
        name: String,
        reason: String,
    },
    Enable {
        name: String,
    },
    Export {
        name: String,
        out: Option<PathBuf>,
        copy: bool,
        to: Option<String>,
    },
    Import {
        archive: PathBuf,
        name: Option<String>,
        settings: Option<PathBuf>,
        opts: UpdateOpts,
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
        Ok(None) => return ExitCode::SUCCESS,
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
            if err.is_network_error() {
                ExitCode::from(EX_TEMPFAIL)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

/// sysexits.h; lets service managers retry only transient failures.
const EX_TEMPFAIL: u8 = 75;

/// Ok(None) means: help was requested and printed.
fn parse_args() -> std::result::Result<Option<Cli>, lexopt::Error> {
    let mut parser = lexopt::Parser::from_env();
    let mut config: Option<PathBuf> = None;

    // Global options until the first positional argument (the command).
    let command = loop {
        match parser.next()? {
            Some(Long("config")) => config = Some(parser.value()?.into()),
            Some(Long("help")) | Some(Short('h')) => {
                print!("{USAGE}");
                return Ok(None);
            }
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
    let mut out = None;
    let mut dry_run = false;
    let mut purge = false;
    let mut name_opt = None;
    let mut reason = None;
    let mut copy = false;
    let mut to = None;
    while let Some(arg) = parser.next()? {
        match arg {
            Long("help") | Short('h') => {
                print!("{}", command_help(&command).unwrap_or(USAGE));
                return Ok(None);
            }
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
            Short('o') | Long("output-file") => out = Some(PathBuf::from(parser.value()?)),
            Long("dry-run") => dry_run = true,
            Long("purge") => purge = true,
            Long("name") => name_opt = Some(parser.value()?.string()?),
            Short('m') | Long("message") => reason = Some(parser.value()?.string()?),
            Long("copy") => copy = true,
            Long("to") => to = Some(parser.value()?.string()?),
            Long("replace") => opts.replace = true,
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
        "update" => {
            if flake.is_some() {
                if names.len() != 1 {
                    return Err("update --flake expects exactly one service name".into());
                }
                opts.flake = flake.clone();
            }
            Cmd::Update { names, opts }
        }
        "boot" => Cmd::Boot,
        "deploy" => Cmd::Deploy {
            name: one_name(&names)?,
            svc: Box::new(ServiceConfig {
                flake: flake.clone().ok_or("deploy requires --flake")?,
                output: output.unwrap_or_else(|| ServiceConfig::default().output),
                ..Default::default()
            }),
            settings,
            opts,
        },
        "activate" => match &names[..] {
            [name, path] => Cmd::Deploy {
                name: name.clone(),
                svc: Box::new(ServiceConfig {
                    prebuilt: Some(path.into()),
                    ..Default::default()
                }),
                settings: None,
                opts,
            },
            _ => return Err("activate expects <name> <store path>".into()),
        },
        "remove" => Cmd::Remove {
            name: one_name(&names)?,
            purge,
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
        "export" => {
            let out = (!dry_run).then(|| out.unwrap_or_else(|| "-".into()));
            if out.as_deref() == Some(Path::new("-")) && std::io::stdout().is_terminal() {
                return Err(
                    "refusing to write an archive to a terminal, use -o <file> or a pipe".into(),
                );
            }
            Cmd::Export {
                name: one_name(&names)?,
                out,
                copy,
                to,
            }
        }
        "disable" => Cmd::Disable {
            name: one_name(&names)?,
            reason: reason.unwrap_or_else(|| "disabled by operator".into()),
        },
        "enable" => Cmd::Enable {
            name: one_name(&names)?,
        },
        "import" => Cmd::Import {
            archive: one_name(&names)
                .map_err(|_| "expected one archive path")?
                .into(),
            name: name_opt,
            settings,
            opts,
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
    let mgr = match &cli.config {
        ConfigSource::File(path) => {
            if cli.config_explicit && !path.exists() {
                return Err(Error::io(format!("cannot read config {}", path.display()))(
                    std::io::ErrorKind::NotFound.into(),
                ));
            }
            let config = flakelet_core::Config::load(path)?;
            // `check --config <rendered>` is the CI entry point. That config
            // describes another machine whose state dir is not ours.
            if cli.config_explicit && matches!(cli.command, Cmd::Check { .. } | Cmd::Driver { .. })
            {
                Manager::off_machine(config)
            } else {
                Manager::new(config)
            }
        }
        // Off-machine: build the machine's rendered config.json from its flake.
        ConfigSource::Machine { flake, name } => {
            let path = Nix::new(&flakelet_core::Config::default(), None).build_attr(&format!(
                "{flake}#nixosConfigurations.{name}.config.services.flakelets.configFile"
            ))?;
            Manager::off_machine(flakelet_core::Config::load(&path)?)
        }
    };
    match &cli.command {
        Cmd::Update { names, opts } => {
            let names = if names.is_empty() {
                for removed in mgr.reconcile()? {
                    println!("{removed}: removed (no longer configured)");
                }
                mgr.service_names()?
            } else {
                names.clone()
            };
            return Ok(update_all(&mgr, &names, opts.clone()));
        }
        Cmd::Boot => {
            let (linked, failed) = mgr.boot()?;
            for name in linked {
                println!("{name}: units re-linked");
            }
            for (name, err) in &failed {
                eprintln!("{name}: error: {err}");
            }
            return Ok(failed.is_empty());
        }
        Cmd::Deploy {
            name,
            svc,
            settings,
            opts,
        } => {
            let mut svc = svc.clone();
            if let Some(p) = settings {
                svc.settings = read_settings(p)?;
            }
            let outcome = mgr.deploy(name, &svc, opts.clone())?;
            println!("{name}: {}", describe(&outcome));
            return Ok(success(&outcome));
        }
        Cmd::Remove { name, purge } => {
            let left = mgr.remove(name, *purge)?;
            println!("{name}: removed");
            if !*purge {
                for p in left {
                    eprintln!("{name}: state left in {} (--purge deletes it)", p.display());
                }
            }
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
        Cmd::Disable { name, reason } => {
            mgr.disable(name, reason)?;
            println!("{name}: disabled");
        }
        Cmd::Enable { name } => match mgr.enable(name)? {
            Some(g) => println!("{name}: enabled, running generation {g}"),
            None => println!("{name}: enabled, nothing deployed yet, run 'flakelet update {name}'"),
        },
        Cmd::Export {
            name, out: None, ..
        } => {
            let (meta, _) = mgr.export_meta(name)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&meta).expect("meta is serializable")
            );
        }
        Cmd::Export {
            name,
            out: Some(out),
            copy,
            to,
        } => {
            mgr.export(name, out, *copy, to.as_deref())?;
            if out.as_os_str() != "-" {
                eprintln!("{name}: exported to {}", out.display());
            }
            if !*copy {
                eprintln!("{name}: disabled here, 'flakelet enable {name}' undoes that");
            }
        }
        Cmd::Import {
            archive,
            name,
            settings,
            opts,
        } => {
            let settings = settings.as_deref().map(read_settings).transpose()?;
            let (name, outcome) = mgr.import(archive, name.as_deref(), settings, opts.clone())?;
            match &outcome {
                UpdateOutcome::Updated { generation } => {
                    println!("{name}: imported as generation {generation}")
                }
                other => println!("{name}: {}", describe(other)),
            }
            return Ok(success(&outcome));
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

fn read_settings(path: &Path) -> Result<Value> {
    let data =
        fs::read_to_string(path).map_err(Error::io(format!("read settings {}", path.display())))?;
    serde_json::from_str(&data).map_err(Error::json(format!("parse settings {}", path.display())))
}

fn update_all(mgr: &Manager, names: &[String], opts: UpdateOpts) -> bool {
    let mut ok = true;
    for name in names {
        match mgr.update(name, opts.clone()) {
            Ok(outcome) => {
                println!("{name}: {}", describe(&outcome));
                ok &= success(&outcome);
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
        let state = if s.disabled.is_some() {
            "disabled"
        } else if s.held.is_some() {
            "held"
        } else if s.degraded {
            "degraded"
        } else if s.updating {
            "updating"
        } else if !s.failed_units.is_empty() {
            "failed"
        } else if s.generation.is_some() {
            "ok"
        } else {
            "not deployed"
        };
        let mark = if let Some(over) = &s.override_flake {
            format!("\t(override {over})")
        } else if s.pin.is_some() {
            "\t(pinned)".into()
        } else {
            String::new()
        };
        println!(
            "{}\t{}\tgen {}\t{}{}",
            s.name,
            state,
            s.generation.map_or("-".into(), |g| g.to_string()),
            s.flake,
            mark
        );
        if let Some(d) = &s.disabled {
            println!("\t{}", d.reason);
            if d.by == DisabledBy::Export && s.origin == Origin::Declarative {
                println!("\tstill declared on this host, remove it from the configuration");
            }
        }
        if let Some(err) = s.held.as_deref().or(s.last_error.as_deref()) {
            let lines: Vec<&str> = err
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            let i = lines
                .iter()
                .rposition(|l| l.contains("error:"))
                .unwrap_or(0);
            let mut line = lines.get(i).copied().unwrap_or(err).to_string();
            if line.ends_with(':') {
                if let Some(next) = lines.get(i + 1) {
                    line = format!("{line} {next}");
                }
            }
            println!("\tlast error: {line}");
        }
        if !s.failed_units.is_empty() {
            println!("\tfailed: {}", s.failed_units.join(" "));
        }
        if !names.is_empty() {
            for u in &s.unit_states {
                println!("\t{}\t{} ({})", u.unit, u.active, u.sub);
            }
        }
        for claim in &s.missing_providers {
            eprintln!("{}: warning: no provider announces '{claim}'", s.name);
        }
    }
    Ok(())
}

fn success(outcome: &UpdateOutcome) -> bool {
    !matches!(
        outcome,
        UpdateOutcome::RolledBack { .. } | UpdateOutcome::Held { .. }
    )
}

fn describe(outcome: &UpdateOutcome) -> String {
    match outcome {
        UpdateOutcome::UpToDate => "up to date".into(),
        UpdateOutcome::Updated { generation } => format!("updated to generation {generation}"),
        UpdateOutcome::Degraded { reason } => {
            format!("degraded, kept current generation: {reason}")
        }
        UpdateOutcome::Held { reason } => format!("held after previous failure: {reason}"),
        UpdateOutcome::Disabled { reason } => format!("disabled, left alone: {reason}"),
        UpdateOutcome::RolledBack { reason } => format!("deploy failed, rolled back: {reason}"),
    }
}
