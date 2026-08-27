# flakelet

Run systemd services from Nix flakes and update them independently of the
host system.

**Status: beta.** Functional and running production services, but the
interface may still change.

Normally a NixOS host has to be rebuilt to update one of its services.
flakelet takes the same approach as `virtualisation.oci-containers`, but with
flakes instead of container images. The host only declares which flake to run
and which settings to pass. The machine itself resolves the flake, evaluates
and builds it, and switches the systemd units. There are no images and no
registry. The units run straight out of the host's nix store.

## Host configuration

```nix
{
  inputs.flakelet.url = "github:Mic92/flakelet";

  # in a nixosConfiguration:
  imports = [ flakelet.nixosModules.flakelet ];
  services.flakelets = {
    enable = true;
    services.myservice = {
      flake = "github:example/my-service";   # or prebuilt = <store path>;
      settings = { port = 8080; };           # passed to the service module
      autoUpdate.enable = true;              # periodic re-evaluation
    };
  };
}
```

Run `flakelet update myservice` on the machine, or let the generated timer do
it. The update evaluates the flake against the host's nixpkgs, builds plain
unit files, links them into `/run/systemd/system` and starts them. Every
update becomes a generation with gc roots. If activation or the service's
health probe fails, flakelet rolls back to the previous generation.

Secrets never go through settings. Pass paths to host-managed secret files
instead, for example from sops-nix, and load them in the unit with
`LoadCredential=`.

## Writing a service

```console
$ nix flake init -t github:Mic92/flakelet
```

A service flake exports an [adios](https://github.com/adisbladis/adios)-style
module: declared `options` for what the host may pass, and an `impl`
returning the units to run in a typed, NixOS-style interface. Hardening is
ordinary systemd configuration such as `DynamicUser=` and `StateDirectory=`.

```nix
flakelets.default = { types, ... }: {
  options.port = { type = types.number; default = 8000; description = "listen port"; };

  impl = { options, pkgs, name, ... }: {
    services.${name} = {
      # no [Install] section: only started when the socket is hit
      serviceConfig.ExecStart = "${pkgs.myservice}/bin/serve";
      serviceConfig.DynamicUser = true;
    };
    sockets.${name} = {
      socketConfig.ListenStream = options.port;
      wantedBy = [ "sockets.target" ];
    };
  };
};
```

The host settings are checked against the options before `impl` is
evaluated: unknown keys, wrong types and missing required settings fail the
update with the offending name.

Units with an `[Install]` section (`wantedBy`) are enabled and started on
activation. Units without one are left to systemd's on-demand activation: a
socket-activated service starts on the first connection, a timer's job runs
on its schedule, not at deploy time. Changed units that are running are
restarted either way.

The impl can also return `exports`, free-form metadata like claimed ports
or metrics endpoints. flakelet publishes the exports of the running
generation to `/run/flakelet/exports/<name>.json`, where firewall,
reverse-proxy or monitoring tooling can pick them up.

The template in `templates/service/flake.nix` shows all of this. DESIGN.md
describes the full contract.

## Health checks

Use systemd for readiness and liveness. `Type=notify` or `ExecStartPost=`
fail the start job if the service does not come up. A failed start job
rolls the activation back. `Restart=` and `WatchdogSec=` keep the service
alive afterwards.

For an end-to-end probe, return `healthCheck`. flakelet runs it as
`<name>-health.service` after every activation. If it fails, flakelet
rolls back:

```nix
impl = { options, pkgs, name, ... }: {
  services.${name} = { … };

  healthCheck = pkgs.writeShellScript "${name}-health" ''
    exec ${pkgs.curl}/bin/curl -sf --retry 5 --retry-connrefused --retry-delay 2 \
      http://127.0.0.1:${toString options.port}/ -o /dev/null
  '';
};
```

`healthCheck` expands to a oneshot that runs as the main unit's user with
`TimeoutStartSec=1min`. That is not always enough. The probe may need
credentials or a longer timeout. In that case write `services.health`
yourself:

```nix
services.health = {
  serviceConfig = {
    Type = "oneshot";
    ExecStart = "${pkgs.myservice}/bin/selftest";
    DynamicUser = true;
    TimeoutStartSec = "5min";
    LoadCredential = "token:/run/secrets/${name}-token";
  };
};
```

Rerun it by hand with `systemctl start myservice-health`.

## State

flakelet reads state from the unit. `StateDirectory=` is copied.
`User=`/`DynamicUser=` owns it. `CacheDirectory=`, `RuntimeDirectory=`
and `LogsDirectory=` are left behind.

This moves a service with its data to another machine:

```console
$ flakelet export web | ssh hostb flakelet import -
```

`export` stops the units. It archives the state folders, the artifact and
the settings. Then it starts the service again. `import` unpacks the
archive and fixes ownership. Then it activates the service and runs the
health probe.

Some state cannot be copied as-is. To serialise it before the copy,
return `dumpScript`. To load it afterwards, return `restoreScript`. Both
run as the main unit's user with its `StateDirectory=`. The other units
are stopped while they run:

```nix
impl = { pkgs, name, ... }: {
  services.${name}.serviceConfig = {
    ExecStart = "${pkgs.myservice}/bin/serve --db /var/lib/${name}/db.sqlite";
    DynamicUser = true;
    StateDirectory = name;
  };

  # flush the WAL so the copied file is consistent
  dumpScript = pkgs.writeShellScript "${name}-dump" ''
    ${pkgs.sqlite}/bin/sqlite3 /var/lib/${name}/db.sqlite 'PRAGMA wal_checkpoint(TRUNCATE);'
  '';
  # rebuild derived data instead of shipping it
  restoreScript = pkgs.writeShellScript "${name}-restore" ''
    ${pkgs.myservice}/bin/reindex --db /var/lib/${name}/db.sqlite
  '';
};
```

`dumpScript` and `restoreScript` generate a `<name>-dump.service` and
`<name>-restore.service` oneshot for you. If you need other unit settings,
define `services.dump` or `services.restore` yourself instead, as shown
for `services.health` above.

## CLI

```
flakelet update [<name>…]        evaluate, build and activate
flakelet status [--json]         generation, degraded/held state, lock holders
flakelet diff <name>             closure diff: running generation vs. fresh eval
flakelet rollback <name>         previous generation
flakelet export <name> [-o f]    stop, archive state to stdout, start again
flakelet import <f>|- [--name n] restore an export here and start it
flakelet remove [--purge] <name> stop a service; --purge also empties its state folders
flakelet reconcile               remove services dropped from the host config
flakelet lock/unlock <name>      pin to the currently deployed revision
flakelet deploy <name> --flake <ref> --settings s.json    imperative service
flakelet activate <name> <path>  start a prebuilt artifact, no evaluation
flakelet check [--build] [--machine <host>]               CI: evaluate/build off-machine
flakelet build <name>… [--out-link <dir>]                 like check, with result symlinks
flakelet gc [--keep <n>]         prune old generations
```

The check command also works away from the machine. For example,
`flakelet check --machine eve --build` evaluates the flakelet configuration
of `nixosConfigurations.eve` in the current flake and builds all of its
service artifacts. Run it in CI to catch broken services before they reach
the machine and to fill the binary cache.

## Contracts and providers

Blessed contracts live in [contracts/](contracts/) as JSON Schema, with
eval-time constructors in the injected `contracts`. Known implementations:

| Contract      | Implementation                                                    | export/import |
| ------------- | ----------------------------------------------------------------- | ------------- |
| `http/v1`     | [flakelet-nginx](https://github.com/Mic92/flakelet-nginx)         | stateless     |
| `postgres/v1` | [flakelet-postgres](https://github.com/Mic92/flakelet-postgres)   | not yet       |

## Real-world examples

- [nixbot](https://github.com/Mic92/nixbot) ships a flakelet module in
  [`nix/flakelet.nix`](https://github.com/Mic92/nixbot/blob/main/nix/flakelet.nix)
  and deploys itself from its own CI via a push effect
  ([`herculesCI/default.nix`](https://github.com/Mic92/nixbot/blob/main/herculesCI/default.nix)).
- [Mic92/dotfiles](https://github.com/Mic92/dotfiles) runs it on eve:
  [`machines/eve/modules/nixbot.nix`](https://github.com/Mic92/dotfiles/blob/main/machines/eve/modules/nixbot.nix)
  wires the service, nginx routing, postgres provisioning and the CI
  deploy trigger.

## Development

```console
$ nix develop
$ cargo test
$ nix build .#checks.x86_64-linux.vm -L   # end-to-end NixOS VM test
```

Design notes live in [DESIGN.md](DESIGN.md).
