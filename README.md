# flakelet

Run systemd services from Nix flakes and update them independently of the
host system.

**Status: alpha.** The design and the on-disk formats may still change. It
has not run production services yet.

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
update becomes a generation with gc roots. If the service's health check
fails, flakelet rolls back to the previous generation.

Secrets never go through settings. Pass paths to host-managed secret files
instead, for example from sops-nix, and load them in the unit with
`LoadCredential=`.

## Writing a service

```console
$ nix flake init -t github:Mic92/flakelet
```

A service flake exports a function. It receives the host's `pkgs`, the entry
`name` and the `settings` from the host configuration. It returns the unit
files to run, as plain derivations. Hardening is ordinary systemd
configuration such as `DynamicUser=` and `StateDirectory=`.

The function can also return a `healthCheck` script, which runs after every
activation and triggers the rollback when it fails. It can return `exports`,
free-form metadata like claimed ports or metrics endpoints. flakelet publishes
the exports of the running generation to `/run/flakelet/exports/<name>.json`,
where firewall, reverse-proxy, monitoring or backup tooling can pick them up.

The template in `templates/service/flake.nix` shows all of this. PLAN.md
describes the full contract.

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

The check command also works away from the machine. For example,
`flakelet check --machine eve --build` evaluates the flakelet configuration
of `nixosConfigurations.eve` in the current flake and builds all of its
service artifacts. Run it in CI to catch broken services before they reach
the machine and to fill the binary cache.

## Development

```console
$ nix develop
$ cargo test
$ nix build .#checks.x86_64-linux.vm -L   # end-to-end NixOS VM test
```

Design notes live in [PLAN.md](PLAN.md).
