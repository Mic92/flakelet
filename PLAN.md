# flakelet — Design

Runtime-managed native systemd services from Nix flakes: flake refs are
evaluated and built on the target machine (like
`virtualisation.oci-containers`, but with flakes instead of container
images), units run straight from the nix store. Services update
independently of the host closure.

Lives in `pkgs/flakelet/` (Rust workspace + NixOS module); moves to its own
repo later.

## Components

1. `flakelet` — Rust CLI, no daemon. Called by generated systemd units and
   manually.
2. NixOS module `services.flakelets` — renders `/etc/flakelet/config.json`,
   generates units/timers.
3. Injected Nix libraries: [adios](https://github.com/adisbladis/adios)
   (with korora types) for typed service modules. That is all v1 supplies;
   a richer `flakelet.lib` comes later (see Follow-ups).
4. Service flakes (external repos), no inputs required.

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

- Dependency-injected: `pkgs` (host nixpkgs, one shared instance), `adios`,
  `name`, optional host helper modules (`services.flakelets.extraModules`).
- Settings are validated by the declared adios/korora types; a type error
  aborts before anything is built, the old generation stays attached.
- Multi-instance: several host entries may point at the same flake with
  different settings; each has its own state, generations, pin and timer.
  flakelet refuses to attach if two managed services (or an existing
  attachment) would produce the same image basename.
- An output attr may be one flakelet module or an attrset of modules. Each
  `impl` returns `{ units = { "<name>-….service" = <derivation>; … };
  healthCheck ? <derivation>; exports ? …; }` — plain unit files referencing
  store paths directly. `healthCheck` is an executable derivation shipped by
  the service itself (it knows how to probe itself); built and gc-rooted with
  the generation. All
  units of one entry form one generation (activated/rolled back atomically).
- Unit names must start with the entry `name`; flakelet enforces this and
  refuses collisions with units of other services or foreign units.
- Supported unit types: `.service`, `.socket`, `.target`, `.timer`, `.path`.
  Mounts and users are host concerns (referenced via settings) — use
  `DynamicUser`/`StateDirectory` where possible.
- No image/container layer: isolation and hardening come from ordinary unit
  directives (`DynamicUser=`, `ProtectSystem=`, `PrivateTmp=`, …); the nix
  store is shared with the host, so nothing is copied or duplicated.

### Activation of native units

"Attach" = symlink the generation's unit files into `/run/systemd/system/`,
`systemctl daemon-reload`, `enable --now` (respecting the units' `[Install]`
sections); "detach" = stop/disable, remove symlinks, reload. `/run` does not
survive reboot: the `flakelet-boot.service` oneshot re-links the current
generation's units before `multi-user.target` without eval or network.
`/etc/systemd/system` is not touched (owned by NixOS activation).

## Evaluation: generated driver expression (pure)

flakelet renders a driver expression, adds it to the store and evaluates it
with nix-eval-jobs. Verified to work without `--impure`:

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

Update steps per service:

1. Read the definition (config.json entry or service.json), verify referenced
   store paths exist, resolve the flake ref to a locked URL + rev + narHash
   (`nix flake metadata`; a pin wins). `input_overrides` become
   `--override-input`/locked-ref rewrites.
2. Render the driver (nixpkgs/adios paths from config.json, settings embedded
   as Nix values, fully locked getFlake refs), `nix store add` it and gc-root
   it — errors reference this path so the exact expression can be inspected
   and re-run.
3. `nix-eval-jobs --force-recurse <driver>`: one evaluator run for the whole
   batch; per-attr errors are independent.
4. `nix build <drvPath>^* --out-link <tmp gcroot>` (never leave fresh outputs
   unrooted), then create the generation dir with `manifest.json` and gcroot
   symlinks for unit files (and their closures), exports derivations,
   soft-referenced settings paths and the flake source + inputs
   (`nix flake archive`) so re-evals work offline.

Eval/build run as the fixed system user `flakelet` (stable owner for the
shared cache `/var/cache/flakelet`), building via nix-daemon; only systemctl
steps run as root.

Resource limits: nix-eval-jobs runs with a configurable `--workers` (default
1; the batch shares one nixpkgs instance anyway) and `--max-memory-size`
derived from available RAM unless set explicitly; the flakelet oneshot units
get `MemoryHigh`/`MemoryMax` and `Nice`/`IOSchedulingClass=idle` so eval and
build cannot starve the machine. Builds inherit nix-daemon's
`max-jobs`/`cores`.

Store paths inside settings have no string context, so the image does not
depend on them (verified). flakelet gc-roots them per generation and fails
early on dangling paths. For build-time content a service can use
fetchTree+narHash (later a `storePath` helper); note this re-adds the content
under a new store path.

## Files

`/etc/flakelet/config.json` — rendered by the module, world-readable, no
secrets. Declarative services with inline settings:

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

### Fetch credentials (private flakes)

Runtime resolution of private repos uses the `credentials` block; values are
plain file paths so any secrets tool works (sops-nix, clan vars, agenix) —
files must be readable by the `flakelet` eval user. Applied to all nix
invocations (metadata, archive, eval, build):

- `netrc_file` → `--option netrc-file`.
- `access_tokens_file` → read by flakelet and passed via `NIX_CONFIG`
  (never on the command line).
- `ssh_key_file` / `ssh_known_hosts_file` → `GIT_SSH_COMMAND="ssh -i … -o
  IdentitiesOnly=yes -o UserKnownHostsFile=… -o StrictHostKeyChecking=yes"`.
- Missing files are reported like dangling settings paths. Per-service
  `credentials` overrides the global block.

`/var/lib/flakelet/<name>/service.json` — manual services (created by
`flakelet deploy`, settings read from `--settings` and stored inline): one
entry of `services.<name>` above.

### Service artifact (self-describing build result)

The driver builds one store path per service, and that artifact is the unit
the rest of the system operates on:

```
/nix/store/…-flakelet-<name>/
  meta.json      # schema version, name, flake_url + rev, settings hash
  units/…        # unit files (settings baked in)
  health-check   # optional executable
  exports.json   # optional (drvs replaced by out paths)
```

The update flow is therefore: produce the artifact (evaluate the driver — or
be handed one) → activate it (gc-root as generation, link units, health
check, publish exports). `flakelet activate <store path>` is a first-class
command; a service entry may set `prebuilt` (module option) instead of
`flake`+`settings` — the two are mutually exclusive. Prebuilt artifacts are
used by CI-primed deploys and tests that must not run nix at runtime;
provenance stays visible via `meta.json` in `flakelet status`.

`/var/lib/flakelet/<name>/state.json` — written by flakelet, atomic
(tmp+rename):

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

`/nix/var/nix/gcroots/flakelet/<name>/gen-<N>/` — `root-*` gcroot symlinks
plus `manifest.json`:

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

`/run/flakelet/exports/<name>.json` — exports of the currently attached
generation, re-published on attach/detach/remove.

Generation dirs are the single mechanism for rollback, gc and multi-unit
atomicity; default `keep_generations = 5`.

All files carry a `"version": 1` field. flakelet reads older versions and
migrates them on the next write; unknown newer versions are a hard error.
Corrupt/truncated state.json or manifest.json: treat the service as
never-deployed for read-only commands, refuse mutating commands without
`--force` (which rebuilds state from the newest intact generation dir).

## Declarative vs. manual services

- Desired set = config.json entries ∪ service.json dirs; on name collision
  the declarative definition wins (with a warning).
- `flakelet remove <name>`: detach, prune generations, delete state.
- `flakelet reconcile` (also implied by `flakelet update` without names):
  removes services whose state says origin=declarative but which are gone
  from config.json. Manual services are never touched. The module runs
  reconcile via a unit triggered on config.json changes.

## Update flow

0. Boot: `flakelet-boot.service` re-links the current generations into
   `/run/systemd/system` (no eval, no network) and sweeps stale
   flakelet-managed symlinks left by crashes; ordered before
   `flakelet.target`, which the generated units and other host units can
   depend on. Offline: if a later eval fails with network errors, the oneshot
   keeps the current units, records "degraded" and exits 0; a manual
   `flakelet update` reports the error as a failure.
1. Skip if held and settings hash + flake rev unchanged (anti-flapping).
2. Eval + build the new generation. On failure: keep current, record error,
   exit non-zero.
3. Unchanged and active: done.
4. Switch: link the new generation's units into `/run/systemd/system/`,
   daemon-reload, restart changed units, enable/start new ones, then
   stop/disable and unlink units that disappeared. If activation fails,
   switch back to the previous generation's units.
5. Health check: no unit of the service may be in `failed` state
   (socket-/timer-activated services are allowed to be inactive); if the
   module returned a `healthCheck` derivation, run it — non-zero exit means
   unhealthy (retry/timeout logic lives inside the check itself).
6. On failure: switch back to the previous generation (its settings are baked
   in), set hold, exit non-zero.
7. On success: write state, publish exports, prune old generations.

Divergence after rollback is visible in `flakelet status --json` and via the
failing oneshot during `nixos-rebuild switch`. Recovery: change host config
(hash changes → hold clears) or `flakelet update --force`.

On `nixos-rebuild switch`, `flakelet-reconcile.service` is ordered before the
per-service update units so renamed/removed services are detached before new
names are activated. Note the host-nixpkgs coupling: bumping the host's
nixpkgs changes the injected `pkgs`, so the next update rebuilds and restarts
every flakelet — usually desired (security fixes); pin
`input_overrides.nixpkgs` per service to opt out.

## Concurrency

- Per-service flock `<state_dir>/<name>/lock` around mutating ops; blocks
  with holder info, `--no-wait` fails immediately (timers).
- Global lock `<state_dir>/lock`: shared for service ops, exclusive for
  `flakelet gc`. Lock order: global → service.
- `status`/`diff` are lock-free; report "updating" if a lock is held.
- Crash safety: flocks auto-release; a killed update leaves at most an unused
  generation dir, cleaned by the next update or gc.

## Checking & CI (multi-machine)

The driver/eval code path also works off-machine:

```
flakelet check  --machine eve [foo…]      # eval only
flakelet check  --build --machine eve     # additionally build the units/closures
flakelet build  --machine eve foo         # local ./result out-link
flakelet driver --machine eve foo         # print the driver expression
flakelet check  --config <config.json>    # any rendered config (CI)
```

`--machine <name>` (default `--flake .`) evaluates
`nixosConfigurations.<name>.config.environment.etc."flakelet/config.json"`
and reuses the normal driver path — no state, locks or attach. Refs resolve
at check time; `--override-ref` pins revisions (PR CI). Wrapping this as
flake checks lets buildbot build all machines' flakelets and prime the binary
cache. Manual services can only be checked on their machine.

## CLI

```
flakelet update [--force] [--no-wait] [<name>…]
flakelet check [--build] [--machine <m> | --config <file>] [<name>…]
flakelet build --machine <m> <name>
flakelet driver [--machine <m>] [<name>…]
flakelet deploy <name> --flake <ref> [--settings <file>] […]
flakelet remove <name>
flakelet reconcile
flakelet status [--json]
flakelet rollback <name>
flakelet diff <name>            # nix store diff-closures current vs. new eval
flakelet lock <name> / unlock <name>
flakelet gc [--keep N]
```

## Crates

Workspace: `flakelet-core` (config/state types, driver generation, locking,
eval/build/attach/rollback/gc, health checks; blocking, no global state) and
`flakelet` (thin clap binary). A future web service links `flakelet-core`;
the file locks make CLI + service safe on one machine. Shell out to `nix`,
`nix-eval-jobs`, `systemctl`.

Tests: unit tests for config/state serde, store-path scanning, driver
generation, hold logic, lock ordering, schema migration. NixOS VM test:
activate, settings change → restart, broken update → rollback + hold,
gc retention, reconcile after rename, boot relink, port-collision refusal,
degraded/offline path, manual deploy/remove.

## Host modules (NixOS first, modular for other platforms)

The module is split so nix-darwin and system-manager can be supported later
(on darwin a systemd subset will be ported):

- `modules/common.nix` — platform-agnostic: the `services.flakelets.*`
  options and rendering of `/etc/flakelet/config.json`.
- `modules/nixos.nix` — systemd wiring: eval user, dirs, `flakelet-boot`,
  `flakelet-reconcile`, per-service oneshots/timers, `flakelet.target`.
- later: `modules/darwin.nix`, `modules/system-manager.nix` reusing
  common.nix and only replacing the unit wiring.

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

- Create user `flakelet`, `/var/cache/flakelet`, `/var/lib/flakelet`.
- Install `flakelet-boot.service` (re-link current generations at boot).
- Render config.json (host nixpkgs + adios paths come from the host's flake
  inputs and stay in the host closure until rooted per generation).
- Per service: oneshot `flakelet-<name>.service` running
  `flakelet update <name>`, restartTriggers on a hash of that entry, ordered
  after `flakelet-reconcile.service` (which triggers on config.json changes);
  optional autoUpdate timer. `flakelet-boot.service` + `flakelet.target` for
  boot ordering.

Update triggers: host config change → restartTriggers; upstream flake change
→ timer or manual update.

## Exports (monitoring, state, discovery)

`exports` is free-form metadata returned next to the image, stored in the
manifest and published to `/run/flakelet/exports/<name>.json`. Derivations in
exports are built, gc-rooted with the generation and replaced by their out
paths — consumers execute store paths matching the running generation.

Blessed schemas (services declare *what* they provide; policy — zones,
domains, TLS, who may reach it — is host-side and lives in the consumers):

- `exports.metrics = [ { port; path ? "/metrics"; scheme ? "http"; } ]`
- `exports.ports.<name> = { port | { from; to; }; protocol ? "tcp";
  internal ? false; }` — non-HTTP listeners. flakelet core validates that no
  two managed services claim the same port before activation. Services that
  want the host to own the socket ship a `.socket` unit instead.
- `exports.http.<name> = { host; upstream; paths ? [ "/" ];
  websockets ? false; …hints }` — reverse-proxy declaration, webserver
  agnostic. Prefer unix-socket upstreams (`RuntimeDirectory=`) over
  localhost ports: no collisions, composes with DynamicUser. Purely proxied
  services need no `exports.ports` entry.
- `exports.state.<name> = { folders; preBackup; postBackup; preRestore;
  postRestore; }` (clan-style; `folders` are host paths, e.g. the unit's
  `StateDirectory`; all hooks are derivations and optional — e.g. preBackup
  dumps a database, postBackup undoes it, preRestore stops/quiesces,
  postRestore restarts)

flakelet only publishes this data. Consumers are separate projects/flakes:
Prometheus `file_sd`/telegraf/Alloy rendering, firewall integration
(nftables named sets / ufw rules updated on attach/detach), reverse-proxy
integration (Caddy admin API, nginx snippet + reload, …), backup adapters
(clan borgbackup/localbackup). flakelet's own health is visible via unit
state, `status --json` and journal MESSAGE_IDs.

## Users & state ownership

- Default: `DynamicUser=` + `StateDirectory=` — zero setup, no interaction
  with host user management.
- If that doesn't fit (postgres peer auth, shared sockets/files, stable
  ownership): the user declares a static system user on the host
  (`users.users.<name>.isSystemUser = true`) and the unit sets `User=` +
  `StateDirectory=`. Runtime user creation (sysusers fragments) is
  deliberately not supported — it conflicts with `users.mutableUsers = false`.
- systemd chowns `StateDirectory` to the unit's user on start, which covers
  migrations (dynamic → static user, import on a machine with different
  uids). Paths outside `StateDirectory` use an `ExecStartPre=+chown …`
  (`+` = run as root) escape hatch. `flakelet export/import` stores names,
  not uids.

## Secrets

Nothing flakelet-specific: secret values never go through settings or units
(world-readable store paths). Settings carry host file paths (sops-nix/clan
vars); units consume them with `LoadCredential=` (preferred),
`BindReadOnlyPaths=` or `LoadCredentialEncrypted=`. Non-store paths are
existence-checked but not gc-rooted.

## Trust (v1)

Only self-controlled flakes; no signature verification; isolation is
whatever the units configure (DynamicUser, sandboxing directives) plus the
unprivileged eval user. `flakelet lock` for pinning.

## Out of scope

- Routing / reverse proxy / port exposure.
- Flake signature verification.
- Canary / side-by-side deployments.

## Follow-ups

- Move to its own repo once stable.
- `flakelet.lib` as its own library: `mkService` — a typed, NixOS-compatible
  unit interface (`services.<name>.serviceConfig`, `sockets`, `timers`, …
  with NixOS naming and rendering; auto `name-` prefix; hard error on unknown
  keys; raw `units` escape hatch) so existing modules port by copy-paste —
  plus `storePath` helper and shared hardening/exporter modules.
- `flakelet export` / `import`: archive of locked flake URL + settings +
  exports + declared state folders (no store paths, no secrets), wrapped in the
  preBackup/postBackup hooks; import registers a manual service and runs
  preRestore → restore folders → postRestore before starting units. Covers
  migration, cloning, disaster recovery.
- `flakelet options <name>`: render the adios option docs of a service.
- Author UX: flake template (`nix flake init -t flakelet#service`), local
  test loop (`flakelet build --flake ./. --settings test.json`), contract
  README.
- Backup adapter consuming `exports.state` (clan borgbackup/localbackup).
- Firewall and reverse-proxy integrations consuming `exports.ports` /
  `exports.http` (separate projects, webserver/firewall agnostic core).
- Automatic port allocation (service requests "one tcp port", flakelet
  assigns from a range and feeds it back via settings).
- Web service on top of `flakelet-core` for remote deploy triggers.
