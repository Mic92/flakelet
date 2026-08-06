# flakelet — Design

flakelet runs native systemd services from Nix flakes. The flake reference is
resolved, evaluated and built on the target machine, similar to
`virtualisation.oci-containers` but with flakes instead of container images.
The units run straight from the nix store. Services update independently of
the host closure.

## Components

1. `flakelet`, a Rust CLI without a daemon. It is called by generated systemd
   units and manually.
2. The NixOS module `services.flakelets`. It renders
   `/etc/flakelet/config.json` and generates the update units and timers.
3. Injected Nix libraries. Version 1 only supplies
   [adios](https://github.com/adisbladis/adios) with korora types for typed
   service modules. A richer `flakelet.lib` comes later, see Follow-ups.
4. The service flakes themselves. They live in external repositories and need
   no inputs.

## Service contract

```nix
{
  outputs = _: {
    # `name` is the host-side entry name; deriving pname/units/state dirs from
    # it makes the flakelet multi-instance capable.
    flakelets.default = { pkgs, adios, name, ... }:
      adios {
        inherit name;
        options = {
          port    = { type = adios.types.int; default = 3000; };
          tlsCert = { type = adios.types.option adios.types.string; default = null; };
        };
        impl = { options, ... }: {
          units = {
            "${name}.service" = pkgs.writeText "${name}.service" ''
              [Service]
              ExecStart=${pkgs.myservice}/bin/serve --port ${toString options.port}
              DynamicUser=true
              StateDirectory=${name}
              [Install]
              WantedBy=multi-user.target
            '';
          };
        };
      };
  };
}
```

The service function is dependency-injected. It receives the host's `pkgs`
(one shared nixpkgs instance), `adios`, the entry `name` and optional host
helper modules from `services.flakelets.extraModules`.

Settings are validated by the declared adios/korora types. A type error
aborts the update before anything is built and the old generation stays
active.

Services are multi-instance capable. Several host entries may point at the
same flake with different settings. Each entry has its own state,
generations, pin and timer.

An output attribute may hold one flakelet module or an attrset of modules.
Each module returns `units`, an optional `healthCheck` and optional
`exports`. The units are plain unit files that reference store paths
directly. The health check is an executable derivation shipped by the
service, because the service knows best how to probe itself. It is built and
gc-rooted together with the generation. All units of one entry form one
generation and are activated or rolled back atomically.

Unit names must start with the entry name. flakelet enforces this and refuses
unit names that already belong to another managed service or to the host.
Supported unit types are `.service`, `.socket`, `.target`, `.timer` and
`.path`. Mounts and users are host concerns and are referenced via settings.
Prefer `DynamicUser=` and `StateDirectory=`.

There is no image or container layer. Isolation and hardening come from
ordinary unit directives such as `DynamicUser=`, `ProtectSystem=` and
`PrivateTmp=`. The nix store is shared with the host, so nothing is copied or
duplicated.

### Activation of native units

Activation symlinks the generation's unit files into `/run/systemd/system/`,
runs `systemctl daemon-reload` and enables and starts the units according to
their `[Install]` sections. Deactivation stops and disables the units,
removes the symlinks and reloads systemd again. `/run` does not survive a
reboot, so the `flakelet-boot.service` oneshot re-links the current
generation's units before `multi-user.target` without evaluation or network
access. `/etc/systemd/system` belongs to NixOS activation and is never
touched.

## Evaluation: a generated driver expression, evaluated purely

flakelet renders a driver expression, adds it to the store and evaluates it
with nix-eval-jobs. This works without `--impure`:

```nix
# /nix/store/…-flakelet-driver.nix (generated per update)
let
  pkgs  = import /nix/store/…-nixpkgs-source { system = "x86_64-linux"; };
  adios = import /nix/store/…-adios;
in {
  grafana = (builtins.getFlake
      "github:me/grafana-svc/<rev>?narHash=sha256-…").flakelets.default {
    inherit pkgs adios;
    name = "grafana";
    settings = { port = 3000; tlsCert = "/nix/store/…-cert.pem"; };
  };
  matrix = …;   # several services batch into one driver, sharing the pkgs instance
}
```

An update of one service runs these steps:

1. Read the definition from config.json or service.json and verify that
   referenced store paths exist. Resolve the flake reference to a locked URL
   with revision and narHash using `nix flake metadata`. A pin set by
   `flakelet lock` wins. `input_overrides` become locked-reference rewrites.
2. Render the driver expression. The nixpkgs and adios paths come from
   config.json and the settings are embedded as Nix values. Add the file to
   the store with `nix store add` and gc-root it. Errors reference this store
   path, so the exact expression can be inspected and re-run.
3. Evaluate the driver with nix-eval-jobs. One evaluator run covers the whole
   batch and per-attribute errors stay independent.
4. Build the derivation with an out-link, so fresh outputs are never left
   without a gc root. Then create the generation directory with
   `manifest.json` and gc-root symlinks for the unit files, the exports
   derivations, soft-referenced settings paths and the flake source with all
   its inputs (via `nix flake archive`). The rooted sources allow re-evaluation
   while offline.

Evaluation and fetching run as the fixed system user `flakelet`, which owns
the shared cache in `/var/cache/flakelet`. Builds go through the nix daemon.
Only the systemctl steps run as root.

nix-eval-jobs runs with a configurable number of workers (default 1, since
the batch shares one nixpkgs instance anyway) and a memory limit derived from
the available RAM unless configured explicitly. The flakelet oneshot units
get `MemoryHigh`, `Nice` and `IOSchedulingClass=idle`, so evaluation and
build cannot starve the machine. Builds inherit `max-jobs` and `cores` from
the nix daemon.

Store paths inside settings carry no string context, so the artifact does not
depend on them. flakelet gc-roots them per generation and fails early when
they are dangling. For build-time content a service can use fetchTree with a
narHash, at the cost of re-adding the content under a new store path. A
`storePath` helper may come later.

## Files

`/etc/flakelet/config.json` is rendered by the module. It is world-readable
and contains no secrets. Declarative services live here with their settings
inline:

```jsonc
{
  "eval_user": "flakelet",
  "cache_dir": "/var/cache/flakelet",
  "state_dir": "/var/lib/flakelet",
  "gcroot_dir": "/nix/var/nix/gcroots/flakelet",
  "nixpkgs": "/nix/store/…-source",
  "adios": "/nix/store/…-adios-source",
  "extra_modules": ["/nix/store/…-mycorp-lib.nix"],
  "eval": { "workers": 1, "max_memory_mb": null },   // null = derive from available RAM
  "credentials": {                                    // all optional, all file paths
    "netrc_file": "/run/secrets/flakelet-netrc",
    "access_tokens_file": "/run/secrets/flakelet-access-tokens",  // "github.com=ghp_…" per line
    "ssh_key_file": "/run/secrets/flakelet-ssh-key",
    "ssh_known_hosts_file": "/etc/ssh/ssh_known_hosts"
  },
  "services": {
    "grafana": {
      "flake": "github:me/grafana-svc",
      "output": "flakelets.default",
      "settings": { "port": 3000, "tlsCert": "/run/secrets/grafana-tls" },
      "input_overrides": { "nixpkgs": "github:NixOS/nixpkgs/nixos-25.05" },
      "keep_generations": 5,
      "credentials": null                  // optional per-service override of the global block
    }
  }
}
```

### Fetch credentials for private flakes

Runtime resolution of private repositories uses the `credentials` block. The
values are plain file paths, so any secrets tool works, for example sops-nix,
clan vars or agenix. The files must be readable by the `flakelet` eval user.
The credentials apply to all nix invocations: metadata, archive, evaluation
and build.

- `netrc_file` is passed as the nix `netrc-file` option.
- `access_tokens_file` is read by flakelet and passed via `NIX_CONFIG`. The
  tokens never appear on a command line.
- `ssh_key_file` and `ssh_known_hosts_file` are turned into a
  `GIT_SSH_COMMAND` with `IdentitiesOnly` and strict host key checking.

Missing credential files are reported like dangling settings paths. A
per-service `credentials` block overrides the global one.

`/var/lib/flakelet/<name>/service.json` holds a manually deployed service. It
is created by `flakelet deploy` and stores the settings from `--settings`
inline. Its format is one entry of `services.<name>` above.

### The service artifact, a self-describing build result

The driver builds one store path per service. This artifact is what the rest
of the system operates on:

```
/nix/store/…-flakelet-<name>/
  meta.json      # schema version, name, flake_url + rev, settings hash
  units/…        # unit files (settings baked in)
  health-check   # optional executable
  exports.json   # optional (drvs replaced by out paths)
```

An update therefore has two halves. First produce the artifact, either by
evaluating the driver or by being handed one. Then activate it: gc-root it as
a generation, link the units, run the health check and publish the exports.
`flakelet activate <name> <store path>` is a first-class command. A service
entry may set `prebuilt` in the module instead of `flake` and `settings`; the
two are mutually exclusive. Prebuilt artifacts serve CI-primed deployments
and tests that must not run nix at runtime. Their provenance stays visible
through `meta.json` in `flakelet status`.

`/var/lib/flakelet/<name>/state.json` is written by flakelet atomically via a
temporary file and rename:

```jsonc
{
  "origin": "declarative" | "manual",
  "generation": 4,                          // null if never deployed
  "units": { "grafana.service": "/nix/store/…-grafana.service" },
  "locked_url": "github:me/grafana-svc/<rev>",
  "pin": null,                              // set by `flakelet lock`
  "hold": { "reason": "…", "settings_hash": "sha256-…", "flake_rev": "<rev>" },
  "degraded": false,                        // cached generation after offline eval failure
  "last_error": null
}
```

`/nix/var/nix/gcroots/flakelet/<name>/gen-<N>/` contains the `root-*` gcroot
symlinks and a `manifest.json`:

```jsonc
{
  "units": { "grafana.service": "/nix/store/…-grafana.service" },
  "flake_url": "github:me/grafana-svc/<rev>?narHash=…",
  "flake_rev": "<rev>",
  "settings_hash": "sha256-…",
  "driver": "/nix/store/…-flakelet-driver.nix",
  "exports": { "metrics": [], "state": {} },   // derivations replaced by out paths
  "created": 1767000000
}
```

`/run/flakelet/exports/<name>.json` holds the exports of the currently active
generation. It is re-published on every activation, at boot and on removal.

Generation directories are the single mechanism for rollback, garbage
collection and multi-unit atomicity. The default is `keep_generations = 5`.

All files carry a `"version": 1` field. flakelet reads older versions and
migrates them on the next write. An unknown newer version is a hard error. A
corrupt or truncated state.json or manifest.json makes read-only commands
treat the service as never deployed. Mutating commands refuse to run without
`--force`, which rebuilds the state from the newest intact generation
directory.

## Declarative and manual services

The desired set of services is the union of the config.json entries and the
service.json directories. On a name collision the declarative definition wins
and flakelet warns about it. `flakelet remove <name>` deactivates the units,
prunes the generations and deletes the state. `flakelet reconcile` removes
services whose state says they were declarative but which no longer appear in
config.json. Manual services are never touched by reconcile. The module runs
reconcile through a unit that restarts when config.json changes, and
`flakelet update` without names implies it.

## Update flow

At boot, `flakelet-boot.service` re-links the current generations into
`/run/systemd/system` without evaluation or network access and sweeps stale
flakelet-managed symlinks left behind by crashes. It is ordered before
`flakelet.target`, which the generated units and other host units can depend
on. When a later evaluation fails with network errors, the update oneshot
keeps the current units, records the service as degraded and exits zero. A
manual `flakelet update` reports the same situation as a failure.

A regular update then proceeds as follows:

1. Skip the update if the service is held and neither the settings hash nor
   the flake revision changed. This prevents flapping retries.
2. Evaluate and build the new generation. On failure keep the current one,
   record the error and exit non-zero.
3. If the result is unchanged and already active, stop here.
4. Switch: link the new generation's units into `/run/systemd/system`, reload
   systemd, restart changed units, enable and start new ones, and finally
   stop, disable and unlink units that disappeared. If activation fails,
   switch back to the previous generation's units.
5. Health check: no unit of the service may be in the `failed` state.
   Socket- and timer-activated services are allowed to be inactive. If the
   module returned a `healthCheck` derivation, run it. A non-zero exit means
   unhealthy. Retry and timeout logic lives inside the check itself.
6. On failure switch back to the previous generation, whose settings are
   baked into its unit files, set a hold and exit non-zero.
7. On success write the state, publish the exports and prune old generations.

Divergence after a rollback is visible in `flakelet status --json` and
through the failing oneshot during `nixos-rebuild switch`. Recovery happens
by changing the host configuration, which changes the hash and clears the
hold, or by running `flakelet update --force`.

During `nixos-rebuild switch`, `flakelet-reconcile.service` is ordered before
the per-service update units, so renamed or removed services are deactivated
before new names are activated. There is a coupling to the host's nixpkgs:
bumping it changes the injected `pkgs`, so the next update rebuilds and
restarts every flakelet. That is usually desired because it delivers security
fixes. A service can opt out by pinning `input_overrides.nixpkgs`.

## Concurrency

Mutating operations take a per-service flock at `<state_dir>/<name>/lock`.
The lock file records the holder, waiting is the default and `--no-wait`
fails immediately, which the timers use. A global lock at `<state_dir>/lock`
is taken shared by service operations and exclusively by `flakelet gc`. The
lock order is global first, then service. `status` and `diff` are lock-free
and report a service as updating when its lock is held. Crash safety comes
from flocks releasing automatically. A killed update leaves at most an unused
generation directory, which the next update or gc removes.

## Checking and CI across machines

The driver and evaluation code path also works away from the machine:

```
flakelet check  --machine eve [foo…]      # eval only
flakelet check  --build --machine eve     # additionally build the units/closures
flakelet build  --machine eve foo         # out-links in the current directory
flakelet driver --machine eve foo         # print the driver expression
flakelet check  --config <config.json>    # any rendered config (CI)
```

`--machine <name>` builds
`nixosConfigurations.<name>.config.services.flakelets.configFile` from a
flake (default: the current directory) and reuses the normal driver path. No
state, locks or activation are involved. Flake references resolve at check
time; `--override-ref` for pinning revisions in PR CI is a follow-up.
Wrapping this as flake checks lets buildbot build the flakelets of every
machine and prime the binary cache. Manual services can only be checked on
their machine.

## CLI

```
flakelet update [--force] [--no-wait] [<name>…]
flakelet check [--build] [--machine <m> | --config <file>] [<name>…]
flakelet build [--machine <m> | --config <file>] <name>…
flakelet driver [--machine <m>] [<name>…]
flakelet deploy <name> --flake <ref> [--settings <file>] […]
flakelet activate <name> <store path>
flakelet remove <name>
flakelet reconcile
flakelet status [--json]
flakelet rollback <name>
flakelet diff <name>            # nix store diff-closures current vs. new eval
flakelet lock <name> / unlock <name>
flakelet gc [--keep N]
```

## Crates

The workspace has two crates. `flakelet-core` contains the config and state
types, driver generation, locking, evaluation, build, activation, rollback,
gc and health checks. It is blocking and has no global state. `flakelet` is a
thin CLI on top, parsed with lexopt. A future web service can link
`flakelet-core`; the file locks make the CLI and such a service safe on one
machine. External work is shelled out to `nix`, `nix-eval-jobs` and
`systemctl`.

Unit tests cover config and state serialization, store-path scanning, driver
generation, hold logic, lock ordering and schema migration. The NixOS VM test
covers activation, settings changes with restarts, broken updates with
rollback and hold, gc retention, reconcile after a rename, boot relinking,
port-collision refusal, the degraded offline path and manual deploy and
remove.

## Host modules

The module is split so nix-darwin and system-manager can be supported later.
On darwin a subset of the systemd wiring will be ported.

- `modules/common.nix` is platform-agnostic. It defines the
  `services.flakelets.*` options and renders `/etc/flakelet/config.json`.
- `modules/nixos.nix` does the systemd wiring: eval user, directories,
  `flakelet-boot`, `flakelet-reconcile`, the per-service oneshots and timers
  and `flakelet.target`.
- `modules/darwin.nix` and `modules/system-manager.nix` come later and reuse
  common.nix, replacing only the unit wiring.

### NixOS module

```nix
services.flakelets.extraModules = [ ./mycorp-lib.nix ];
services.flakelets.services.<name> = {
  flake = "github:me/foo";
  output = "flakelets.default";
  settings = { ... };
  inputOverrides = { };
  autoUpdate = { enable = false; interval = "daily"; };
  keepGenerations = 5;
};
services.flakelets.eval = { workers = 1; maxMemoryMb = null; };
services.flakelets.credentials = { netrcFile = null; accessTokensFile = null;
                                   sshKeyFile = null; sshKnownHostsFile = null; };
```

The module creates the `flakelet` user, `/var/cache/flakelet` and
`/var/lib/flakelet`. It installs `flakelet-boot.service` and renders
config.json; the host's nixpkgs and adios paths come from the host's flake
inputs and stay in the host closure until they are rooted per generation.
Every service gets a oneshot `flakelet-<name>.service` that runs
`flakelet update <name>`, restarts when the entry changes and is ordered
after `flakelet-reconcile.service`. Enabling `autoUpdate` adds a timer.
Updates are therefore triggered by host configuration changes through the
restart triggers, and by upstream flake changes through the timer or a manual
update.

## Exports for monitoring, state and discovery

`exports` is free-form metadata that the service module returns next to its
units. It is stored in the manifest and published to
`/run/flakelet/exports/<name>.json`. Derivations inside exports are built,
gc-rooted with the generation and replaced by their output paths, so
consumers always execute store paths that match the running generation.

The blessed schemas describe what a service provides. Policy such as zones,
domains, TLS and who may reach the service stays on the host side, in the
consumers:

- `exports.metrics = [ { port; path ? "/metrics"; scheme ? "http"; } ]`
- `exports.ports.<name> = { port | { from; to; }; protocol ? "tcp";
  internal ? false; }` describes non-HTTP listeners. flakelet refuses to
  activate a service whose port claims collide with another managed service.
  Services that want the host to own the socket ship a `.socket` unit
  instead.
- `exports.http.<name> = { host; upstream; paths ? [ "/" ];
  websockets ? false; …hints }` is a webserver-agnostic reverse-proxy
  declaration. Prefer unix-socket upstreams via `RuntimeDirectory=` over
  localhost ports; they cannot collide and compose with `DynamicUser=`.
  Purely proxied services need no `exports.ports` entry.
- `exports.state.<name> = { folders; preBackup; postBackup; preRestore;
  postRestore; }` follows the clan model. `folders` are host paths such as
  the unit's `StateDirectory`. All hooks are optional derivations. For
  example, preBackup dumps a database, postBackup cleans up, preRestore
  quiesces the service and postRestore starts it again.

flakelet only publishes this data. The consumers are separate projects:
Prometheus file_sd, telegraf or Alloy rendering, firewall integration through
nftables sets or ufw rules, reverse-proxy integration through the Caddy admin
API or nginx snippets, and backup adapters such as clan borgbackup. The
health of flakelet itself is visible through unit state, `status --json` and
journal MESSAGE_IDs.

## Users and state ownership

The default is `DynamicUser=` with `StateDirectory=`. It needs no setup and
does not interact with host user management. When that does not fit, for
example for postgres peer authentication, shared sockets or stable ownership,
the host declares a static system user and the unit sets `User=` and
`StateDirectory=`. Creating users at runtime through sysusers fragments is
deliberately not supported because it conflicts with
`users.mutableUsers = false`.

systemd chowns the `StateDirectory` to the unit's user on start. That covers
migrations from a dynamic to a static user and imports on a machine with
different uids. Paths outside the `StateDirectory` can use an
`ExecStartPre=+chown …` escape hatch, where the `+` runs the command as root.
The future `flakelet export` and `import` store user names, not uids.

## Secrets

Nothing here is flakelet-specific. Secret values never go through settings or
unit files, because both end up in the world-readable store. Settings carry
host file paths managed by sops-nix or clan vars, and units consume them with
`LoadCredential=`, `BindReadOnlyPaths=` or `LoadCredentialEncrypted=`.
Non-store paths are checked for existence but not gc-rooted.

## Trust in version 1

Only run flakes you control. There is no signature verification. Isolation is
whatever the units configure through `DynamicUser=` and sandboxing
directives, plus the unprivileged eval user. Use `flakelet lock` to pin a
revision.

## Out of scope

- Routing, reverse proxies and port exposure.
- Flake signature verification.
- Canary and side-by-side deployments.

## Follow-ups

- `flakelet.lib` as its own library. Its `mkService` offers a typed,
  NixOS-compatible unit interface with `services.<name>.serviceConfig`,
  `sockets`, `timers` and NixOS naming and rendering, an automatic name
  prefix, hard errors on unknown keys and the raw `units` escape hatch, so
  existing NixOS modules port by copy and paste. It also gains a `storePath`
  helper and shared hardening and exporter modules.
- `flakelet export` and `import`. The export is an archive of the locked
  flake URL, the settings, the exports and the declared state folders,
  without store paths or secrets, wrapped in the preBackup and postBackup
  hooks. Import registers a manual service and runs preRestore, restores the
  folders and runs postRestore before starting the units. This covers
  migration, cloning and disaster recovery.
- `flakelet options <name>` to render the adios option documentation of a
  service.
- `flakelet check --override-ref` to pin revisions in PR CI.
- `flakelet diff` using `nix store diff-closures` between the current and the
  new evaluation.
- A backup adapter consuming `exports.state`, for clan borgbackup and
  localbackup.
- Firewall and reverse-proxy integrations consuming `exports.ports` and
  `exports.http`, as separate projects with a webserver- and
  firewall-agnostic core.
- Automatic port allocation: a service requests one tcp port, flakelet
  assigns it from a range and feeds it back through settings.
- A web service on top of `flakelet-core` for remote deploy triggers.
