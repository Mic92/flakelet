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

On the host:

```nix
imports = [ flakelet.nixosModules.flakelet ];
services.flakelets = {
  enable = true;
  services.web = {
    flake = "github:example/web";
    settings = { port = 8080; };
    autoUpdate.enable = true;
  };
};
```

In the service repository (`nix flake init -t github:Mic92/flakelet`):

```nix
flakelets.default = { types, ... }: {
  options.port = { type = types.number; default = 8000; };
  impl = { options, inputs }: let inherit (inputs.nixpkgs) pkgs; inherit (inputs.flakelet) name; in {
    services.${name} = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig.ExecStart = "${pkgs.web}/bin/serve --port ${toString options.port}";
      serviceConfig.DynamicUser = true;
      serviceConfig.StateDirectory = name;
    };
  };
};
```

`flakelet update web` on the machine, or the generated timer, evaluates the
flake against the host's nixpkgs, builds plain unit files, links them into
`/run/systemd/system` and starts them. Every update is a generation with gc
roots. If activation or the service's health probe fails, flakelet rolls
back. `flakelet export web | ssh hostb flakelet import -` moves it, state
included, to another machine.

## Documentation

Guides

- [Writing a service](docs/guides/writing-a-service.md): options, units,
  health checks, state, exports, secrets
- [Host setup](docs/guides/host-setup.md): NixOS module, private flakes,
  CI, prebuilt artifacts, day-to-day commands
- [Moving a service](docs/guides/moving-a-service.md): export and import

Reference

- [Service module](docs/reference/service-module.md): everything `impl`
  may receive and return
- [Host options](docs/reference/host-options.md): `services.flakelets.*`
- [CLI](docs/reference/cli.md)
- [Contracts](docs/reference/contracts.md): export schemas and providers
- [Files on the machine](docs/reference/files.md)

Background

- [Design](docs/design.md): why it works the way it does

## Providers

| Contract      | Implementation                                                    | export/import |
| ------------- | ----------------------------------------------------------------- | ------------- |
| `http/v1`     | [flakelet-nginx](https://github.com/Mic92/flakelet-nginx)         | stateless     |
| `postgres/v1` | [flakelet-postgres](https://github.com/Mic92/flakelet-postgres)   | yes           |

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
