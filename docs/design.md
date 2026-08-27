# flakelet — Design

This document explains why flakelet works the way it does. How to use it
is in the [guides](guides/), exact formats and options are in the
[reference](reference/).

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
3. `flakelet.lib`, applied by the driver expression. It checks the
   host-provided settings against the module's declared options
   ([adios](https://github.com/adisbladis/adios) declarations over korora
   types: `{ type, default?, description? }`, unknown keys rejected) and
   validates and renders what `impl` returns: a typed, NixOS-style unit
   interface with `services.<name>.serviceConfig`, `sockets` and `timers`,
   an automatic name prefix and hard errors on unknown keys, so existing
   NixOS modules can be ported by copy and paste. Helpers (`contracts`, `storePath`) are injected into `impl`.
4. The service flakes themselves, in the adios module shape:
   `flakelets.<output> = { types, … }: { options = {…}; impl = {…}: {…}; }`.
   adios' `inputs`/`defaultFunc` wiring is not supported: a flakelet is a
   single module whose inputs the driver injects. They live in their own
   repositories and do not need any flake inputs.

## Service contract

The shape is documented in the
[service module reference](reference/service-module.md); here are the
reasons behind it.

`impl` is dependency-injected and service flakes have no inputs. The host
decides which nixpkgs a service is built against, so a host upgrade
delivers security fixes to every service without their authors doing
anything, and one evaluation of nixpkgs is shared by the whole batch.
Everything a service needs beyond `pkgs` must therefore be injectable,
which is why `contracts`, `storePath` and `extraModules` are arguments
rather than libraries to import.

Unit names and directories derive from the injected `name` rather than
being fixed by the flake, so the same flake can be instantiated several
times on one machine, each instance with its own settings, state,
generations, pin and timer.

Settings are type-checked before `impl` runs and unknown keys in the
returned unit tree are hard errors. Both exist for the same reason: a
typo must fail the update loudly while the running generation stays
active, instead of producing a unit without `ExecStart=` that only fails
at start time.

The interface mirrors NixOS' `systemd.services` so existing modules port
by copy and paste, but it renders to plain unit files with settings baked
in. There is no module system fixpoint and nothing to merge with the host.

All units of one entry form one generation. They are activated together and
rolled back together, so a service consisting of a `.service` and a `.socket`
never ends up half-updated.

Entry names consist of letters, digits, `-` and `_`. Unit names must
start with the entry name. flakelet enforces this and refuses to activate a unit name that already belongs to another managed service or to
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

### Health checks are units

An earlier version ran a `healthCheck` executable from flakelet itself.
That re-implemented systemd poorly: hand-rolled privilege dropping, no
timeout (a hanging probe held the service lock forever), no journal, and
it only ran when flakelet activated something. Making the probe a
`<name>-health.service` oneshot gets `TimeoutStartSec=`, sandboxing,
journal and `systemctl start` for free, and because it is part of the
generation it is gc-rooted and rolled back together with the code it
probes. Readiness and liveness stay where systemd already handles them:
the start job and `Restart=`/`WatchdogSec=`.

The probe is shipped by the service rather than configured on the host,
because the service knows best how to probe itself. The `healthCheck`
sugar runs it with the main unit's identity so it can reach `0660`
sockets and read-only self-tests work without spelling the unit out; a
service that wants a black-box probe from an unrelated user writes
`services.health` explicitly.

### State is derived, not declared

A separate state declaration would duplicate what `StateDirectory=` and
`User=`/`DynamicUser=` already say and could drift from it. So
`flakelet.lib` derives the folder list and owners from the structured
`serviceConfig` at evaluation time and writes `state.json` into the
artifact. Core never parses unit files, a CI build sees exactly what the
machine will see, and the folder list of a generation is known before any
data is touched. `CacheDirectory=` and friends are excluded because
systemd's own semantics say they are disposable.

Dump and restore hooks are units for the same reasons the health probe
is. They run with the main unit's identity because their job is to read
and write its state, and they require a `StateDirectory=` because without
one they would have nowhere sandbox-safe to put their output.
`extraFolders` demands a static `User=` because ownership of paths
outside `/var/lib` cannot be fixed up by systemd on start the way
`StateDirectory=` is.

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
from the host, the injected `storePath` helper turns such a string into one with
context (via `builtins.appendContext`, since `builtins.storePath` is banned
in pure evaluation), making the artifact really depend on it.

## Files on the machine

The formats are in the [files reference](reference/files.md). The design
choices:

Everything is plain JSON so an operator can debug with `jq`. config.json
is world-readable, which forces secrets out of settings and into paths.
Credentials for private flakes are file paths too, resolved at runtime by
the eval user, so they compose with sops-nix, clan vars or agenix and
access tokens go through `NIX_CONFIG` rather than a command line visible
in `ps`. Manual services live in their own `service.json` so they survive
host rebuilds that know nothing about them.

Generation directories under the gcroots are the single mechanism behind
rollback, gc and multi-unit atomicity; there is no database to fall out of
sync with them. `/run/flakelet/exports` is re-published on activation,
boot and removal so consumers never read metadata of a generation that is
not running. Files carry a version; flakelet migrates old ones and refuses
newer ones, because guessing about an unknown format is how state gets
destroyed. Corrupt state makes mutating commands demand `--force`, which
rebuilds from the newest intact generation.

### The service artifact

The driver builds exactly one self-describing store path per service.
Structuring the output this way splits every update into two halves that can
be performed independently. The first half produces the artifact, normally by
evaluating the driver on the machine. The second half activates it: gc-root
it as a new generation, link the units, run the health probe, publish the
exports. Because activation only needs the artifact and nothing else, it also
works when someone else already built it. `flakelet activate <name> <store
path>` does exactly that, and a declarative service can set `prebuilt`
instead of `flake` and `settings`; the two are mutually exclusive. This is
how CI-primed deployments and tests avoid running any evaluation on the
target machine, while `meta.json` keeps the provenance visible in
`flakelet status`.

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
into `/run/systemd/system` and queues start jobs for their `[Install]`
units (`--no-block`, since it runs inside the boot transaction, which was
computed before these units existed). It runs before `flakelet.target`, so other host
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

1. Evaluate and build the new generation (or take the prebuilt artifact).
   If this fails, keep the current generation, record the error in the
   state and exit non-zero.
2. If the resulting artifact is the one a previous activation failed on,
   skip it. This is the anti-flapping rule: without it a broken update
   would restart the working service on every timer tick. Keying the hold
   on the artifact covers settings, revision, host nixpkgs and prebuilt
   paths alike; `--force` overrides it.
3. If the artifact is the active generation's, there is nothing to do.
4. Otherwise switch the entry as a whole: publish the new exports (so a
   provider can act on `requires.*` before the units need it), stop,
   disable and unlink all units of the old generation in one job, link the
   new generation's units into `/run/systemd/system`, reload systemd, clear
   any `failed` state left from before, then enable and start the units
   that have an `[Install]` section. Units without one are pulled in on
   demand, so a socket-activated service stays inactive until a connection
   arrives and a timer's job does not fire just because the service was
   deployed. Treating the entry as a unit avoids per-unit ordering problems
   (a socket cannot be restarted while its service runs) and never leaves a
   mix of two generations loaded.
5. Verify health. If the generation ships a `<name>-health.service` probe
   unit, start it and treat a failed start job as unhealthy; systemd provides
   the timeout, the journal entry and the sandbox. Then no unit of the
   service may be in the `failed` state; socket- and timer-activated units
   are allowed to be inactive.
6. If activation or the health probe failed, delete the new generation
   directory (it never ran, so it is no rollback target), re-publish the
   previous exports and switch back to the previous generation. Its unit
   files still exist in the store and have their old settings baked in, so
   this is a pure symlink-and-restart operation. Record a hold with the
   reason and exit non-zero.
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
state, so the same code path can run on a developer laptop or in CI
(`flakelet check`/`build`/`driver`, see the [CLI reference](reference/cli.md)).
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

## Crates

The Rust workspace has two crates. `flakelet-core` is a library containing
the config and state types, driver generation, locking, evaluation, build,
activation, rollback, gc and health probes. It is blocking code with no
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

Options are listed in the [host options reference](reference/host-options.md).
The module creates the `flakelet` user together with `/var/cache/flakelet`
and `/var/lib/flakelet`, installs `flakelet-boot.service` and renders
config.json. The nixpkgs and adios store paths written into the config come
from the host's own flake inputs, so they are part of the host closure and
cannot be garbage-collected before the services root them per generation.

Every configured service gets a oneshot `flakelet-<name>.service` that runs
`flakelet update <name>`. It is `RemainAfterExit`, so a configuration
switch re-runs it only when the service's entry changed, and it is ordered
after `flakelet-reconcile.service` so removals happen before additions.
Enabling `autoUpdate` adds `flakelet-<name>-auto.timer` and a matching
non-remaining oneshot with the configured interval; a per-host stable
random delay (`RandomizedDelaySec` with `FixedRandomDelay`) spreads the
firings across a fleet, so one new revision does not restart the service on
every machine at the same moment. In practice updates
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

A few schemas are blessed (listed in the
[contracts reference](reference/contracts.md)) so consumers can rely on
their shape. The service declares what it provides; policy questions such
as zones, public host names, TLS and who may reach the service stay on the
host side, in the consumers. `ports` is the one export core acts on: two
daemons fighting over one port is better caught before either of them is
restarted, so colliding claims refuse activation.

flakelet only publishes this data. The consumers are separate projects:
Prometheus file_sd, telegraf or Alloy rendering for metrics, nftables sets or
ufw rules for the firewall, the Caddy admin API or nginx snippets for the
proxy. Backup adapters build on the export machinery described below. The health of
flakelet itself needs no exports; it is visible through unit state,
`flakelet status --json` and journal MESSAGE_IDs.

## Contracts and providers

The blessed export schemas generalize into contracts: versioned interfaces
(`http/v1`, `postgres/v1`) between a service and a provider on the host. A
contract has two directions. `provides` is what the service offers — the
exports above. `requires` is a claim a provider must act on: "I need a
postgres database". It travels in the same exports file:

```nix
impl = { options, pkgs, name, ... }: {
  services.${name}.serviceConfig = {
    ExecStart = "${pkg}/bin/serve --db postgresql://${name}@/${name}?host=/run/postgresql";
    User = name;
  };
  exports = {
    http.web = { host = options.domain; upstream = "unix:/run/${name}/web.sock"; };
    requires.postgres = { database = name; role = name; };
  };
};
```

There is no feedback channel from provider to service: contracts are
deterministic, so the consumer bakes the outcome into its config at
evaluation time, as the `ExecStart` above does. `postgres/v1` means local
socket peer authentication — no password exists. Negotiated values
(allocated ports, generated passwords, remote databases) are out of v1 on
purpose; the provision/bind handshake of the Open Service Broker lineage is
the complexity this design avoids.

Providers run on the host, typically as NixOS modules, because they own
host-scope resources: ports 80/443 and certificates for nginx, the superuser
socket for postgres. A provider may also be a flakelet itself. The rules:

- Level-triggered: a path unit wakes the provider, which reconciles from the
  full directory, never from a delta. That absorbs missed events, coalescing
  and provider restarts.

  ```ini
  [Path]
  PathChanged=/run/flakelet/exports
  ```

- Provisioning is idempotent and add-only. Nothing is deprovisioned
  automatically, not even on `flakelet remove`: a rollback must land on a
  generation whose database still exists. Orphans are listed by the provider
  and deleted by humans. Stateless renderings (vhosts) do converge: an
  absent export file tears the route down, which is why `flakelet remove`
  deletes the export file.
- One provider per contract per host.
- Each provider announces its capability:

  ```json
  // /etc/flakelet/providers.d/postgres-v1.json
  { "contract": "postgres/v1" }
  ```

  flakelet does not parse contract schemas; it only warns in `check` and
  `status` when a claim has no announcer. Enforcement stays soft: a missing
  provider surfaces as a failed start, `Restart=` retries until provisioning
  catches up. First-deploy races resolve the same way, not by an ordering
  protocol. Unknown keys in the announcement are ignored.
- A provider that can move the resource it provisions announces how:

  ```json
  { "contract": "postgres/v1",
    "state": { "dump": "/nix/store/…/bin/dump", "restore": "/nix/store/…/bin/restore" } }
  ```

  Both are called by `flakelet export`/`import` as `<hook> <claim.json>
  <dir>` once per `requires.<contract>` claim. What they write into `<dir>`
  is opaque to core. `restore` must create the resource itself if it does
  not exist yet, because on import it runs before the exports file that
  normally triggers provisioning is published, and it refuses a non-empty
  one, which keeps provisioning add-only. This is host tooling talking to a
  host provider at export time, not the provider→service feedback channel
  ruled out above. A provider without `state` makes its consumers
  non-exportable, which `status --json` and `export --dry-run` report.

Contracts inherit the version 1 trust model: services may claim any domain
or database name, which is also what lets blue/green instances share one
database. Providers still validate exports against the schema, catching
mistakes rather than attackers. If multi-tenancy ever arrives, provider-side
grant options are the extension point; flakelet core would not change.

Where a contract lives follows one criterion: whether services reach it
through the injected module arguments (service flakes are input-free, so
anything they need must be injectable) and whether core interprets it.
`ports` and `state` are core-interpreted and cannot move. Pure descriptions
that are near-universal and multi-implementation — `http`, later `metrics`
— are blessed here: JSON Schema in `contracts/`, constructor in
the injected `contracts`. Backing-service contracts such as `postgres` live
in their implementation's repository (`flakelet-postgres`), because their
hard parts — add-only provisioning, orphans, rollback interplay — are
inseparable from the provider, and the family is open-ended (redis,
s3, …) while claims are plain attrsets needing no constructor. The JSON
shape is the interface in all cases; a provider in any language validates
against the schema file, never against Nix code. Schema changes require a
working implementation and a real consumer; recurring `extra.<impl>` keys
are the promotion signal for new typed fields.

Providers are guests in host services, never owners: they extend only through
append-safe merge points (an nginx include directive, SQL-level
provisioning) and must compose with an existing `services.nginx` or
`services.postgresql` configuration. Implementations live in their own
repositories, named for what they wrap (`flakelet-nginx`,
`flakelet-postgres`); the known ones are listed in the README.

## Export and import

Usage is in the [guide](guides/moving-a-service.md), the archive format in
the [files reference](reference/files.md#export-archive). The decisions:

Both need nothing from the service author in the common case, because
folders, owners and provider claims are already derived per generation.
Consistency comes from stopping all units of the entry in one
`systemctl stop`, so timers and sockets cannot re-trigger the service
mid-copy; snapshots are a follow-up, and the archive records
`consistency: "stopped"` to leave room for that.

No store paths travel, because the target can build them, and no secret
contents, because the archive would otherwise need the same protection as
the secrets. Settings do travel so a bare target can reproduce the
service, with host-specific paths listed for replacement rather than
silently reused.

Import pins a freshly registered entry to the exported revision so state
is restored onto the code that wrote it, but defers to an entry the
target already declares, because that is the normal migration case and
the host configuration is the source of truth. It refuses non-empty
target folders rather than merging, checks users and providers before
building, and compares the evaluated `state.json` with the archive before
extracting, so a mismatch is caught before any data lands. Everything
after extraction is ordinary activation, so the health probe and rollback
apply unchanged.

Backup adapters (`flakelet-borgbackup`, a clan `state` bridge) link
`flakelet-core` and reuse the same steps with their own storage, schedule
and retention; they need no contract.

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

State ownership across changes is handled by systemd itself: it chowns (or
id-maps) the `StateDirectory` to the unit's user on start, which covers the
migration from a dynamic to a static user as well as importing state onto a
machine where the uids differ. `flakelet import` therefore extracts
`StateDirectory=` contents root-owned and lets the first start fix them;
only `extraFolders` are chowned by core, to the static `User=`/`Group=`
names recorded at export, which must exist on the target. Paths outside the
`StateDirectory` can fall back to an `ExecStartPre=+chown …` line, where
the leading `+` runs that one command as root.

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
fact that evaluation runs as an unprivileged user. `flakelet lock` pins a
service to the revision of its active generation until you have reviewed
the next one.

## Out of scope

- Routing, reverse proxies and port exposure. flakelet publishes exports;
  acting on them is someone else's job.
- Flake signature verification.
- Canary and side-by-side deployments.

## Follow-ups

- Shared hardening and exporter modules for `flakelet.lib`.
- zfs/btrfs snapshots for export and rollback: a read-only snapshot instead
  of keeping units stopped during the copy, a state snapshot per generation
  with `rollback --with-state` to undo schema migrations, a dataset or
  subvolume per service, `zfs/btrfs send` as export transport, and a
  provider hint that a filesystem snapshot of its data directory is
  consistent. The current design keeps this open: stop → dump → copy →
  start are separate steps, the folder list is known per generation before
  any data is touched, the archive has one member per folder and a
  `consistency` field, `state` in artifact and manifest is an open object,
  flakelet never creates state directories itself, and unknown announcement
  keys are ignored.
- `flakelet options <name>` to render the adios option documentation of a
  service.
- `flakelet check --override-ref` to pin revisions in pull-request CI.
- A backup adapter on top of the export steps, for clan borgbackup and
  localbackup; `state.{dump,restore}` in `flakelet-postgres`.
- Contract implementations: `flakelet-nginx` and `flakelet-postgres`, plus
  firewall integrations consuming `exports.ports`.
- Automatic port allocation: a service asks for one tcp port, flakelet
  assigns it from a range and feeds it back through settings.
- A web service on top of `flakelet-core` for remote deploy triggers.
