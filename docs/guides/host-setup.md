# Host setup

## NixOS module

```nix
{
  inputs.flakelet.url = "github:Mic92/flakelet";

  outputs = { nixpkgs, flakelet, ... }: {
    nixosConfigurations.eve = nixpkgs.lib.nixosSystem {
      modules = [
        flakelet.nixosModules.flakelet
        {
          services.flakelets = {
            enable = true;
            services.web = {
              flake = "github:example/web";
              settings = { port = 8080; };
              autoUpdate.enable = true;
            };
          };
        }
      ];
    };
  };
}
```

After `nixos-rebuild switch`, `flakelet-web.service` evaluates the flake on
the machine, builds the units and starts them. It runs again whenever this
entry changes. With `autoUpdate` a timer re-evaluates `daily` (see
`autoUpdate.interval`) and picks up new revisions of the service flake.

Check what is running:

```console
$ flakelet status
$ systemctl status web.service
```

All module options are listed in the
[host options reference](../reference/host-options.md).

## Several instances of one flake

Units and state directories derive from the entry name, so this works:

```nix
services.flakelets.services = {
  web-blue  = { flake = "github:example/web"; settings.port = 8080; };
  web-green = { flake = "github:example/web/next"; settings.port = 8081; };
};
```

## Secrets

Settings are world-readable in the nix store. Pass paths, not values:

```nix
sops.secrets.web-token.owner = "root";
services.flakelets.services.web.settings.tokenFile =
  config.sops.secrets.web-token.path;
```

The service loads the file with `LoadCredential=`. flakelet checks that
the path exists before deploying.

## Private flakes

Fetching happens on the machine at runtime, as the `flakelet` user. Give
it credentials as files:

```nix
services.flakelets.credentials = {
  # https: either a netrc file …
  netrcFile = config.sops.secrets.flakelet-netrc.path;
  # … or "github.com=ghp_…" lines
  accessTokensFile = config.sops.secrets.flakelet-tokens.path;
  # git+ssh://
  sshKeyFile = config.sops.secrets.flakelet-deploy-key.path;
  sshKnownHostsFile = "/etc/ssh/ssh_known_hosts";
};
sops.secrets.flakelet-netrc.owner = "flakelet";
```

## Static users

`DynamicUser=` needs nothing from the host. When a service sets `User=`
instead, declare the user:

```nix
users.users.matrix = { isSystemUser = true; group = "matrix"; };
users.groups.matrix = { };
services.flakelets.services.matrix = { flake = "github:example/matrix"; };
```

## Pinning nixpkgs for one service

Services are built against the host's nixpkgs. A host upgrade rebuilds and
restarts them. To decouple one service:

```nix
services.flakelets.services.legacy = {
  flake = "github:example/legacy";
  inputOverrides.nixpkgs = "github:NixOS/nixpkgs/nixos-24.11";
};
```

## Prebuilt artifacts

Skip evaluation on the machine and activate something CI built:

```nix
services.flakelets.services.web.prebuilt = inputs.web.packages.x86_64-linux.flakelet-web;
```

or imperatively:

```console
$ flakelet build web --machine eve --out-link ./out
$ nix copy --to ssh://eve ./out/web
$ ssh eve flakelet activate web $(readlink ./out/web)
```

## CI

`flakelet check` evaluates every service of a machine without root and
without touching state. Run it in CI to catch broken services before the
machine does, and add `--build` to fill the binary cache:

```console
$ flakelet check --machine eve            # eval only
$ flakelet check --machine eve --build    # also build
```

`--machine` reads `nixosConfigurations.eve` from the flake in the current
directory (`--flake <ref>` for another one).

## Providers

Services can ask for a reverse-proxy route or a database through
`exports`. The host answers by importing a provider module:

```nix
imports = [
  flakelet-nginx.nixosModules.default      # serves exports.http.*
  flakelet-postgres.nixosModules.default   # provisions exports.requires.postgres
];
```

See the [contracts reference](../reference/contracts.md) for what exists.

## Operating

```console
$ flakelet update web              # deploy now instead of waiting for the timer
$ flakelet diff web                # what would change
$ flakelet rollback web            # previous generation
$ flakelet lock web                # stay on this revision until `unlock`
$ flakelet update web --force      # retry after a failed deploy put it on hold
$ journalctl -u flakelet-web -u web
```

A failed deploy rolls back and puts the service on hold. The hold clears
by itself when the settings or the flake revision change. The full list
of commands is in the [CLI reference](../reference/cli.md).
