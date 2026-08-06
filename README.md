# flakelet

Runtime-managed native systemd services from Nix flakes.

Think `virtualisation.oci-containers`, but with flake references instead of
container images: the host configuration only declares *which* flake to run
and its settings; the target machine resolves, evaluates and builds the
service at runtime and switches its systemd units. Services update
independently of the host closure — no host rebuild, no image registry, no
duplication of the nix store into squashfs images.

```nix
services.flakelets = {
  enable = true;
  services.grafana = {
    flake = "github:example/grafana-flakelet";
    settings.port = 3000;
  };
};
```

`flakelet update grafana` (or the generated timer) resolves the flake,
evaluates it against the host's nixpkgs, builds plain unit files, links them
into `/run/systemd/system` and starts them. Every update is a generation with
gc roots; failed health checks roll back automatically.

## A service flake

```console
$ nix flake init -t github:Mic92/flakelet
```

A service is a function from `{ pkgs, name, settings, ... }` to:

- `units."<name>*.{service,socket,timer,target,path}"` — plain unit file
  derivations (hardening via ordinary unit directives, `DynamicUser=`,
  `StateDirectory=`, …)
- `healthCheck` (optional) — executable derivation, run after activation;
  failure rolls back
- `exports` (optional) — metadata (claimed ports, metrics endpoints, proxy
  hints, state folders) published to `/run/flakelet/exports/<name>.json` for
  firewall/proxy/monitoring/backup integrations

See `templates/service/flake.nix` and PLAN.md for the full contract.

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

Secrets never go through settings: pass host file paths (sops-nix, clan vars)
and consume them in the unit with `LoadCredential=`.

## CLI

```
flakelet update [<name>…]        evaluate, build and activate
flakelet status [--json]         generation, degraded/held state, lock holders
flakelet rollback <name>         previous generation
flakelet lock/unlock <name>      pin to the currently resolved revision
flakelet deploy <name> --flake <ref> --settings s.json    imperative service
flakelet activate <name> <path>  start a prebuilt artifact, no evaluation
flakelet check [--build] [--machine <host>]               CI: evaluate/build off-machine
flakelet build <name>… [--out-link <dir>]                 like check, with result symlinks
flakelet gc [--keep <n>]         prune old generations
```

`flakelet check --machine eve --build --gc-roots-dir ./roots` evaluates the
flakelet config of `nixosConfigurations.eve` from the current flake and builds
all its service artifacts — use it in CI to catch broken services before they
reach the machine and to prime the binary cache.

## Development

```console
$ nix develop
$ cargo test
$ nix build .#checks.x86_64-linux.vm -L   # end-to-end NixOS VM test
```

Design notes live in [PLAN.md](PLAN.md).
