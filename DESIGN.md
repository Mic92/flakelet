# flakelet — Design

flakelet runs native systemd services from Nix flakes. The idea is borrowed
from `virtualisation.oci-containers`: the host configuration only names the
thing to run and the settings to run it with, and the machine itself takes
care of fetching and starting it. Instead of pulling a container image from a
registry, flakelet resolves a flake reference, evaluates and builds it on the
target machine, and switches the resulting systemd units. The units run
straight from the nix store that the host already has.

The point of this indirection is that services can be updated without
rebuilding or redeploying the host system. A service repository can move
faster than the machine configuration, can be owned by a different person or
team, and can be rolled back independently. At the same time nothing about
the runtime environment is container-like: there are no images to build or
mirror, no separate store, and hardening is ordinary systemd configuration.

## Components

The moving parts are deliberately few:

1. `flakelet`, a Rust command line tool. There is no daemon. The generated
   systemd units call it, and an operator can call it manually at any time to
   inspect or fix things.
2. The NixOS module `services.flakelets`. It renders the machine-wide
   configuration file `/etc/flakelet/config.json` and generates the systemd
   units and timers that trigger updates.
3. A small set of Nix libraries that flakelet injects into service modules:
   [adios](https://github.com/adisbladis/adios) together with korora, which
   gives service authors typed options, and `flakelet.lib` (injected as
   `flakeletLib`). Its `mkService` offers a typed, NixOS-style unit interface
   with `services.<name>.serviceConfig`, `sockets` and `timers`, an automatic
   name prefix, hard errors on unknown keys and a raw `units` escape hatch,
   so existing NixOS modules can be ported by copy and paste. It also carries
   the `storePath` helper described below.
4. The service flakes themselves. They live in their own repositories and do
   not need any flake inputs, because everything they need is passed in as
   arguments.

## Service contract

A service flake exports a function under `flakelets.<attr>`. Here is a
complete example using adios for typed settings:

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

The function is dependency-injected. It receives the host's `pkgs`, which is
one nixpkgs instance shared between all services of a batch, the `adios`
library, the entry `name` chosen by the host configuration, the `settings`
from the host, and any helper modules the host wants to hand out through
`services.flakelets.extraModules`. Because the service derives its unit names
and state directory from `name`, the same flake can be instantiated several
times on one machine under different names, each with its own settings,
state, generations, pin and timer.

Settings are validated by the adios/korora types the service declares. A type
error aborts the update before anything is built, which means the currently
running generation simply stays active and the operator sees the error in the
journal and in `flakelet status`.

The value an output attribute holds may be a single flakelet module or an
attrset of them. Each module returns up to three things:

- `units`: an attrset from unit file name to a derivation containing the unit
  file. These are plain systemd units that reference store paths directly;
  the settings are baked into them at evaluation time.
- `healthCheck` (optional): an executable derivation. flakelet runs it after
  every activation. The check is shipped by the service rather than
  configured on the host, because the service knows best how to probe itself.
  It is built and gc-rooted together with the generation, so a rollback also
  rolls back to the matching check.
- `exports` (optional): metadata about what the service provides, described
  in its own section below.

All units of one entry form one generation. They are activated together and
rolled back together, so a service consisting of a `.service` and a `.socket`
never ends up half-updated.

Unit names must start with the entry name. flakelet enforces this and refuses
to activate a unit name that already belongs to another managed service or to
the host itself, because silently overriding someone else's unit would be a
debugging nightmare. The supported unit types are `.service`, `.socket`,
`.target`, `.timer` and `.path`. Mounts and users are host concerns; if a
service needs them it references them through settings. Where possible,
services should rely on `DynamicUser=` and `StateDirectory=` instead.

There is no image or container layer anywhere in this design. Isolation and
hardening come from ordinary unit directives such as `DynamicUser=`,
`ProtectSystem=` and `PrivateTmp=`. Because the nix store is shared with the
host, dependencies that the host already has are not downloaded or stored a
second time.

### Activation of native units

Activation is deliberately boring. flakelet symlinks the generation's unit
files into `/run/systemd/system/`, runs `systemctl daemon-reload`, and
enables and starts the units according to their `[Install]` sections.
Deactivation is the mirror image: stop and disable the units, remove the
symlinks, reload systemd once more.

`/run` is a tmpfs and does not survive a reboot. That is intentional: the
`flakelet-boot.service` oneshot re-creates the symlinks for the current
generation early during boot, before `multi-user.target`, and needs neither
evaluation nor network access to do so. `/etc/systemd/system` belongs to
NixOS activation and flakelet never touches it, so a `nixos-rebuild switch`
and a flakelet update cannot fight over the same files.

## Evaluation: a generated driver expression, evaluated purely

flakelet does not evaluate service flakes directly. Instead it renders a
small "driver" expression, adds that file to the nix store and evaluates it
with nix-eval-jobs. The driver pins everything: the nixpkgs source path comes
from the host configuration, the flake reference is fully locked including
its narHash, and the settings are embedded as Nix values. This is what makes
the evaluation pure; no `--impure` flag is needed anywhere.

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

An update of a service walks through the following steps:

1. Read the service definition, either from config.json for declarative
   services or from service.json for manually deployed ones. Verify that any
   store paths mentioned in the settings actually exist. Resolve the flake
   reference to a locked URL with revision and narHash using
   `nix flake metadata`; if the operator pinned the service with
   `flakelet lock`, the pin wins. An `input_overrides.nixpkgs` entry is
   resolved to a locked reference and replaces the pkgs instance injected
   into that service; other input names are rejected, because
   `builtins.getFlake` cannot rewrite a flake's own lock purely and the
   service contract forbids flake inputs anyway.
2. Render the driver expression and add it to the store with `nix store add`,
   where it is also gc-rooted. Keeping the driver around pays off when
   something goes wrong: every error message references the store path of the
   exact expression that was evaluated, so the operator can open it, read it
   and re-run it by hand.
3. Evaluate the driver with nix-eval-jobs. One evaluator run covers the whole
   batch of services, which amortizes the cost of importing nixpkgs, and an
   evaluation error in one attribute does not affect the others.
4. Build the resulting derivation with an out-link, so a freshly built output
   is never left without a gc root even for a moment. Then create the
   generation directory containing `manifest.json` and gc-root symlinks for
   the unit files, the exports derivations, any store paths referenced from
   the settings, and the flake source together with all of its inputs
   (collected with `nix flake archive`). Rooting the sources means the
   service can be re-evaluated later even when the machine is offline.

Evaluation and fetching run as the fixed system user `flakelet`. Using one
stable user gives the shared evaluation cache in `/var/cache/flakelet` a
consistent owner across updates. Builds go through the nix daemon like any
other build on the machine. Only the systemctl steps at the end run as root.

Evaluating nixpkgs is not free, so the resource usage is bounded on several
levels. nix-eval-jobs runs with a configurable number of workers; the default
is a single worker, because the whole batch shares one nixpkgs instance
anyway and more workers mostly cost memory. Its memory limit is derived from
the available RAM unless configured explicitly. The flakelet oneshot units
run with `MemoryHigh`, `Nice` and `IOSchedulingClass=idle`, so a scheduled
update cannot starve the services that are already running. Builds inherit
`max-jobs` and `cores` from the nix daemon configuration.

One subtlety about settings that contain store paths: a string like
`/nix/store/…-cert.pem` inside the settings has no string context, so the
built artifact does not depend on it and nix would happily garbage-collect
it. flakelet therefore checks that such paths exist before evaluating and
gc-roots them per generation. If a service needs actual build-time content
from the host, `flakeletLib.storePath` turns such a string into one with
context (via `builtins.appendContext`, since `builtins.storePath` is banned
in pure evaluation), making the artifact really depend on it.

## Files on the machine

flakelet keeps its state in a handful of JSON files. They are all plain
files, so an operator can read them with `jq` when something looks odd.

`/etc/flakelet/config.json` is rendered by the NixOS module. It is
world-readable and therefore contains no secrets. Declarative services live
here with their settings inline:

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

Private repositories need credentials at resolution and fetch time, and those
credentials have to be available at runtime on the machine, not just when the
host configuration is built. The `credentials` block solves this with plain
file paths, so it composes with whatever secrets tool the host already uses,
be it sops-nix, clan vars or agenix. The referenced files must be readable by
the `flakelet` eval user, since that is who runs the fetching. The
credentials are applied to every nix invocation flakelet makes: metadata,
archive, evaluation and build.

- `netrc_file` is passed to nix as the `netrc-file` option and covers https
  fetches.
- `access_tokens_file` contains `host=token` lines. flakelet reads it and
  passes the tokens through the `NIX_CONFIG` environment variable, so they
  never show up on a command line or in `ps` output.
- `ssh_key_file` and `ssh_known_hosts_file` are combined into a
  `GIT_SSH_COMMAND` with `IdentitiesOnly` and strict host key checking, which
  covers `git+ssh` flake references.

A missing credential file is reported the same way as a dangling settings
path: the update fails early with a clear message. A per-service
`credentials` block overrides the global one, for the case where one service
needs a different identity than the rest of the machine.

`/var/lib/flakelet/<name>/service.json` holds a manually deployed service,
created by `flakelet deploy`. Its content is one entry of `services.<name>`
from the config above, with the settings from `--settings` stored inline.
Keeping manual services in their own files means they survive host rebuilds
that know nothing about them.

### The service artifact, a self-describing build result

The driver builds exactly one store path per service, and that artifact is
what the rest of the system operates on:

```
/nix/store/…-flakelet-<name>/
  meta.json      # schema version, name, flake_url + rev, settings hash
  units/…        # unit files (settings baked in)
  health-check   # optional executable
  exports.json   # optional (drvs replaced by out paths)
```

Structuring the output this way splits every update into two halves that can
be performed independently. The first half produces the artifact, normally by
evaluating the driver on the machine. The second half activates it: gc-root
it as a new generation, link the units, run the health check, publish the
exports. Because activation only needs the artifact and nothing else, it also
works when someone else already built it. `flakelet activate <name> <store
path>` does exactly that, and a declarative service can set `prebuilt`
instead of `flake` and `settings`; the two are mutually exclusive. This is
how CI-primed deployments and tests avoid running any evaluation on the
target machine, while `meta.json` keeps the provenance visible in
`flakelet status`.

`/var/lib/flakelet/<name>/state.json` records what is currently deployed. It
is written atomically via a temporary file and rename:

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
symlinks and a `manifest.json` describing that generation:

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
generation. It is re-published on every activation, at boot and on removal,
so consumers never read metadata that belongs to a generation which is no
longer running.

Generation directories are the single mechanism behind rollback, garbage
collection and multi-unit atomicity. There is no separate database to fall
out of sync with them. The default retention is `keep_generations = 5`.

All of these files carry a `"version": 1` field. flakelet reads older
versions and migrates them on the next write, and refuses to touch files
written by a newer flakelet, because guessing about an unknown format is how
state gets destroyed. If a state.json or manifest.json is corrupt or
truncated, read-only commands treat the service as never deployed, and
mutating commands refuse to run unless given `--force`, which rebuilds the
state from the newest intact generation directory.

## Declarative and manual services

The desired set of services is the union of the entries in config.json and
the service.json files under `/var/lib/flakelet`. When both define the same
name, the declarative definition wins and flakelet warns about the shadowed
manual one.

`flakelet remove <name>` deactivates the units, prunes all generations and
deletes the state directory. `flakelet reconcile` looks for services whose
state says they were declarative but which no longer appear in config.json,
and removes them; this is what cleans up after a service is renamed or
deleted in the host configuration. Manual services are never touched by
reconcile, since no declarative source of truth exists for them. The module
runs reconcile through a unit that restarts whenever config.json changes, and
`flakelet update` without service names implies a reconcile as well.

## Update flow

Boot comes first. `flakelet-boot.service` re-links the current generations
into `/run/systemd/system` and sweeps any stale flakelet-managed symlinks a
crash may have left behind. It runs before `flakelet.target`, so other host
units that want to order themselves after "the flakelet services are up" can
depend on that target. The boot unit needs no evaluation and no network,
which matters for machines that boot while offline.

Offline behaviour in general follows one rule: an unattended update must not
take a working service down just because the network is flaky. When an
evaluation fails with a network error, the update oneshot keeps the current
units running, marks the service as degraded in its state and exits zero so
the boot or timer run does not report a failure. A human running
`flakelet update` interactively gets the same situation reported as an error,
because a human can actually do something about it.

A regular update proceeds like this:

1. If the service is held from a previous failed deploy and neither the
   settings hash nor the flake revision has changed, skip it. This is the
   anti-flapping rule: without it a broken update would be retried by every
   timer tick.
2. Evaluate and build the new generation. If this fails, keep the current
   generation, record the error in the state and exit non-zero.
3. If the new units are identical to the running ones, there is nothing to
   do.
4. Otherwise switch: first stop, disable and unlink the units that
   disappeared, so a renamed unit releases its ports before its successor
   starts. Then link the new generation's units into `/run/systemd/system`,
   reload systemd, and for every changed or new unit: enable and restart it
   if it has an `[Install]` section; otherwise only try-restart it if it is
   currently running. Units without `[Install]` are pulled in on demand, so a
   socket-activated service stays inactive until a connection arrives and a
   timer's job does not fire just because the service was deployed. If any of
   this fails, switch back to the previous generation's units.
5. Run the health check. First, no unit of the service may be in the `failed`
   state; socket- and timer-activated units are allowed to be inactive. Then,
   if the module shipped a `healthCheck` derivation, execute it and treat a
   non-zero exit as unhealthy. Retry and timeout logic lives inside the check
   itself, where the service author can tune it.
6. If activation or the health check failed, switch back to the previous
   generation. Its unit files still exist in the store and have their old
   settings baked in, so this is a pure symlink-and-restart operation. Record
   a hold with the reason and exit non-zero.
7. On success, write the new state, publish the exports and prune old
   generations beyond the retention limit.

After a rollback the machine intentionally diverges from what the host
configuration asks for. That divergence is visible in
`flakelet status --json` and through the failing oneshot during
`nixos-rebuild switch`. It resolves itself as soon as the inputs change:
either the host configuration changes, which changes the settings hash and
clears the hold, or the service publishes a new revision, or the operator
forces a retry with `flakelet update --force`.

During `nixos-rebuild switch`, `flakelet-reconcile.service` is ordered before
the per-service update units. That way a renamed or removed service is
deactivated before a new name tries to claim its ports or unit names.

One coupling deserves a warning: the injected `pkgs` is the host's nixpkgs.
Bumping the host's nixpkgs therefore changes the inputs of every service, and
the next update rebuilds and restarts all of them. Most of the time this is
exactly what you want, because it is how services pick up security fixes
without their authors doing anything. A service that must not follow the host
can pin its own nixpkgs via `input_overrides.nixpkgs`.

## Concurrency

Several flakelet processes can run at the same time: timers fire while an
operator runs a manual update, or a `nixos-rebuild switch` restarts the
oneshots while a deploy is in flight. File locks keep this safe.

Every mutating operation takes a per-service flock at
`<state_dir>/<name>/lock`. The lock file records who holds it, so a waiting
process can tell the operator what it is waiting for. Waiting is the default;
`--no-wait` makes the attempt fail immediately, which is what the timer units
use so they never pile up. A global lock at `<state_dir>/lock` is taken in
shared mode by normal service operations and exclusively by `flakelet gc`,
because gc must not race an update that is just creating a new generation.
The lock order is always global first, then service, which rules out
deadlocks between the two. `status` and `diff` take no locks at all and
simply report a service as "updating" when its lock is currently held.

Crash safety follows from the same design. Flocks are released automatically
when a process dies. A killed update leaves at most an unused generation
directory behind, which the next update or gc removes.

## Checking and CI across machines

Everything up to activation works without being root and without touching
state, so the same code path can run on a developer laptop or in CI:

```
flakelet check  --machine eve [foo…]      # eval only
flakelet check  --build --machine eve     # additionally build the units/closures
flakelet build  --machine eve foo         # out-links in the current directory
flakelet driver --machine eve foo         # print the driver expression
flakelet check  --config <config.json>    # any rendered config (CI)
```

`--machine <name>` builds
`nixosConfigurations.<name>.config.services.flakelets.configFile` from a
flake, defaulting to the flake in the current directory, and feeds that
rendered config into the normal driver path. Flake references resolve at
check time, so CI sees what a machine would deploy right now; pinning
revisions for pull-request CI via `--override-ref` is a follow-up. Wrapping
these commands as flake checks lets a build farm evaluate and build the
flakelets of every machine and prime the binary cache, so the machines
themselves only download. Manual services can only be checked on their
machine, because only that machine knows about them.

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

The Rust workspace has two crates. `flakelet-core` is a library containing
the config and state types, driver generation, locking, evaluation, build,
activation, rollback, gc and health checks. It is blocking code with no
global state, so it can be embedded elsewhere; a future web service for
remote deploy triggers would link it directly, and the file locks already
make the CLI and such a service safe to run side by side on one machine.
`flakelet` is a thin binary on top, with argument parsing done by lexopt.
Work that nix or systemd already do well is shelled out to `nix`,
`nix-eval-jobs` and `systemctl` rather than reimplemented.

Unit tests cover config and state serialization, store-path scanning, driver
generation, hold logic, lock ordering and schema handling. A NixOS VM test
exercises the real thing end to end: activation, settings changes with
restarts, prebuilt artifacts, broken updates with rollback and hold, gc
retention, reconcile after a rename, boot relinking, port-collision refusal,
the degraded offline path and manual deploy and remove.

## Host modules

The NixOS module is split into a platform-agnostic part and the systemd
wiring, so nix-darwin and system-manager can be added later without touching
the option definitions. On darwin only a subset of the systemd wiring will be
portable.

- `modules/common.nix` defines the `services.flakelets.*` options and renders
  `/etc/flakelet/config.json`.
- `modules/nixos.nix` contains the systemd wiring: the eval user, the state
  and cache directories, `flakelet-boot`, `flakelet-reconcile`, the
  per-service oneshots and timers, and `flakelet.target`.
- `modules/darwin.nix` and `modules/system-manager.nix` come later and reuse
  common.nix, replacing only the wiring.

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

The module creates the `flakelet` user together with `/var/cache/flakelet`
and `/var/lib/flakelet`, installs `flakelet-boot.service` and renders
config.json. The nixpkgs and adios store paths written into the config come
from the host's own flake inputs, so they are part of the host closure and
cannot be garbage-collected before the services root them per generation.

Every configured service gets a oneshot `flakelet-<name>.service` that runs
`flakelet update <name>`. The oneshot restarts whenever the service's entry
in the configuration changes, and it is ordered after
`flakelet-reconcile.service` so removals happen before additions. Enabling
`autoUpdate` adds a timer with the configured interval. In practice updates
are therefore triggered from two directions: a change in the host
configuration triggers the restart of the oneshot, and a new revision of the
service flake is picked up by the timer or by an operator running
`flakelet update` by hand.

## Exports for monitoring, state and discovery

A service usually does not exist in isolation. Something has to open a
firewall port for it, put it behind a reverse proxy, scrape its metrics or
back up its state. flakelet itself does none of these things, but it gives
the service a way to describe what it offers, and it makes that description
available to the tools that do.

That description is `exports`, free-form metadata the service module returns
next to its units. It is stored in the generation's manifest and published to
`/run/flakelet/exports/<name>.json` whenever a generation becomes active. Any
derivations inside the exports are built, gc-rooted with the generation and
replaced by their output paths, so a consumer that executes a hook from the
exports always runs code matching the generation that is actually running.

A few schemas are blessed so consumers can rely on their shape. The service
declares what it provides; policy questions such as zones, public host names,
TLS and who may reach the service stay on the host side, in the consumers:

- `exports.metrics = [ { port; path ? "/metrics"; scheme ? "http"; } ]`
  announces Prometheus-style metrics endpoints.
- `exports.ports.<name> = { port | { from; to; }; protocol ? "tcp";
  internal ? false; }` describes non-HTTP listeners. flakelet refuses to
  activate a service whose port claims collide with those of another managed
  service, because two daemons fighting over one port is better caught before
  either of them is restarted. A service that wants the host to own the
  listening socket ships a `.socket` unit instead of an export.
- `exports.http.<name> = { host; upstream; paths ? [ "/" ];
  websockets ? false; …hints }` is a webserver-agnostic reverse-proxy
  declaration. Unix-socket upstreams via `RuntimeDirectory=` are preferred
  over localhost ports: they cannot collide and they compose with
  `DynamicUser=`. A service that is only reachable through the proxy needs no
  `exports.ports` entry at all.
- `exports.state.<name> = { folders; preBackup; postBackup; preRestore;
  postRestore; }` follows the clan model for stateful data. `folders` are
  host paths such as the unit's `StateDirectory`. The hooks are optional
  derivations: preBackup might dump a database, postBackup removes the dump,
  preRestore quiesces the service and postRestore starts it again.

flakelet only publishes this data. The consumers are separate projects:
Prometheus file_sd, telegraf or Alloy rendering for metrics, nftables sets or
ufw rules for the firewall, the Caddy admin API or nginx snippets for the
proxy, and backup adapters such as clan borgbackup for state. The health of
flakelet itself needs no exports; it is visible through unit state,
`flakelet status --json` and journal MESSAGE_IDs.

## Users and state ownership

The default answer to "which user does my service run as" is `DynamicUser=`
together with `StateDirectory=`. It requires no setup and does not interact
with host user management at all. Some services cannot use it, for example
because they rely on postgres peer authentication, share sockets or files
with other services, or need stable ownership of large state. In that case
the host declares an ordinary static system user in its own configuration and
the unit sets `User=` and `StateDirectory=`. Creating users at runtime
through sysusers fragments was considered and rejected: it conflicts with
`users.mutableUsers = false`, which many hosts set on purpose.

State ownership across changes is handled by systemd itself: it chowns the
`StateDirectory` to the unit's user on start, which covers the migration from
a dynamic to a static user as well as importing state onto a machine where
the uids differ. Paths outside the `StateDirectory` can fall back to an
`ExecStartPre=+chown …` line, where the leading `+` runs that one command as
root. The future `flakelet export` and `import` store user names rather than
uids for the same reason.

## Secrets

Nothing here is specific to flakelet, the rules are the same as for any Nix
deployment. Secret values must never travel through settings or unit files,
because both end up world-readable in the nix store. Instead the settings
carry paths to secret files that the host manages with sops-nix, clan vars or
similar, and the units consume those files with `LoadCredential=`,
`BindReadOnlyPaths=` or `LoadCredentialEncrypted=`. flakelet checks that such
paths exist before deploying but does not gc-root them, since they are not
store paths.

## Trust in version 1

flakelet version 1 assumes you only run flakes you control. There is no
signature verification of any kind. Isolation is exactly what the units
configure through `DynamicUser=` and other sandboxing directives, plus the
fact that evaluation runs as an unprivileged user. `flakelet lock` exists to
pin a service to a known revision until you have reviewed the next one.

## Out of scope

- Routing, reverse proxies and port exposure. flakelet publishes exports;
  acting on them is someone else's job.
- Flake signature verification.
- Canary and side-by-side deployments.

## Follow-ups

- Shared hardening and exporter modules for `flakelet.lib`.
- `flakelet export` and `import`. The export is an archive of the locked
  flake URL, the settings, the exports and the declared state folders,
  without store paths or secrets, wrapped in the preBackup and postBackup
  hooks. Import registers a manual service, runs preRestore, restores the
  folders and runs postRestore before starting the units. Together they cover
  migration, cloning and disaster recovery.
- `flakelet options <name>` to render the adios option documentation of a
  service.
- `flakelet check --override-ref` to pin revisions in pull-request CI.
- A backup adapter consuming `exports.state`, for clan borgbackup and
  localbackup.
- Firewall and reverse-proxy integrations consuming `exports.ports` and
  `exports.http`, as separate projects with a webserver- and
  firewall-agnostic core.
- Automatic port allocation: a service asks for one tcp port, flakelet
  assigns it from a range and feeds it back through settings.
- A web service on top of `flakelet-core` for remote deploy triggers.
