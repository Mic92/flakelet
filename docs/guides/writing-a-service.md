# Writing a service

Start from the template:

```console
$ nix flake init -t github:Mic92/flakelet
```

A service flake exports an [adios](https://github.com/adisbladis/adios)
module under `flakelets.<attr>`. It does not need a nixpkgs flake input:
the host hands in `pkgs` and the entry `name` as adios inputs. Other
flake inputs work as usual.

```nix
{
  outputs = _: {
    flakelets.default = { types, ... }: {
      options.port = {
        type = types.number;
        default = 8000;
        description = "TCP port the HTTP server listens on.";
      };

      impl = { options, inputs }: let
        inherit (inputs.nixpkgs) pkgs;
        inherit (inputs.flakelet) name;
      in {
        services.${name} = {
          description = "${name} example service";
          wantedBy = [ "multi-user.target" ];
          serviceConfig = {
            ExecStart = "${pkgs.python3}/bin/python3 -m http.server ${toString options.port}";
            DynamicUser = true;
            StateDirectory = name;
          };
        };
      };
    };
  };
}
```

`options` declares what the host may pass as `settings`. flakelet checks
the settings against it before `impl` runs. Unknown keys, wrong types and
missing required options fail the update and name the offending key. A
default may depend on other values:
`defaultFunc = { inputs, ... }: "/var/lib/${inputs.flakelet.name}"`.

`impl` receives the checked `options` and two inputs: `inputs.nixpkgs`
(`pkgs`, `lib`) and `inputs.flakelet` (`name`, `contracts`, `storePath`,
[…](../reference/service-module.md#impl--options-inputs-)). Derive unit
names and directories from `name` so the host can run the same flake
twice under different names. The examples below assume the `let inherit
…` from above.

The return value is shaped like NixOS' `systemd.*` options
([full list](../reference/service-module.md)). Unknown keys are rejected,
so a typo like `serviceconfig` fails the update.

Try it locally without a host:

```console
$ echo '{"port": 8080}' > settings.json
$ sudo flakelet deploy web --flake . --settings settings.json
$ flakelet status
$ sudo flakelet remove web
```

## Unit naming and multiple units

The attribute equal to `name` becomes `<name>.service`. Every other
attribute `foo` becomes `<name>-foo.<type>`. So a worker and a timer:

```nix
impl = { options, inputs }: … {
  services.${name} = { … };
  services.worker = {
    serviceConfig.ExecStart = "${pkgs.myservice}/bin/worker";
    serviceConfig.DynamicUser = true;
  };
  timers.gc = {
    timerConfig.OnCalendar = "daily";
    wantedBy = [ "timers.target" ];
  };
  services.gc = {
    serviceConfig.Type = "oneshot";
    serviceConfig.ExecStart = "${pkgs.myservice}/bin/gc";
  };
};
```

renders `web.service`, `web-worker.service`, `web-gc.timer` and
`web-gc.service` for `name = "web"`. All of them form one generation. They
are switched and rolled back together.

## When units start

Units with `wantedBy`/`requiredBy` are enabled and started on activation.
Units without are left to systemd, so a socket-activated service starts on
the first connection and a timer's job runs on schedule, not at deploy
time ([exact rules](../reference/service-module.md#activation-semantics)).

```nix
impl = { options, inputs }: … {
  # no wantedBy: started by the socket
  services.${name}.serviceConfig = {
    ExecStart = "${pkgs.myservice}/bin/serve";
    DynamicUser = true;
  };
  sockets.${name} = {
    socketConfig.ListenStream = options.port;
    wantedBy = [ "sockets.target" ];
  };
};
```


## Health checks

Use systemd for readiness and liveness. `Type=notify` or `ExecStartPost=`
fail the start job if the service does not come up. A failed start job
rolls the activation back. `Restart=` and `WatchdogSec=` keep the service
alive afterwards.

For an end-to-end probe, return `healthCheck`. flakelet runs it as
`<name>-health.service` after every activation. If it fails, flakelet
rolls back:

```nix
impl = { options, inputs }: … {
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
and `LogsDirectory=` are left behind. That is all
[`flakelet export`/`import`](moving-a-service.md) need to move the
service with its data to another machine.

Some state cannot be copied as-is. To serialise it before the copy,
return `dumpScript`. To load it afterwards, return `restoreScript`. Both
run as the main unit's user with its `StateDirectory=`. The other units
are stopped while they run:

```nix
impl = { options, inputs }: … {
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

Prefer `DynamicUser=`. If the service needs a fixed user, for postgres
peer auth or shared files, the host declares it and the unit sets `User=`.
State outside `/var/lib` then goes into `exports.state.extraFolders`:

```nix
services.${name}.serviceConfig = {
  User = name;
  StateDirectory = name;
};
exports.state.extraFolders = [ "/srv/media" ];
```

## Exports

`exports` tells the host what the service offers or needs, for example a
reverse-proxy route and a database:

```nix
impl = { options, inputs }: let inherit (inputs.flakelet) name contracts; in {
  services.${name}.serviceConfig = { … };
  exports = {
    http.web = contracts.http {
      host = options.domain;
      upstream = "unix:/run/${name}/web.sock";
    };
    requires.postgres = { database = name; role = name; };
  };
};
```

Provider modules on the host act on these. The available shapes are in
the [contracts reference](../reference/contracts.md).

## Secrets

Never put secret values into settings. They end up in the nix store. Pass
a path and load it in the unit:

```nix
options.tokenFile = { type = types.string; };
impl = { options, inputs }: … {
  services.${name}.serviceConfig = {
    LoadCredential = "token:${options.tokenFile}";
    ExecStart = "… --token-file \${CREDENTIALS_DIRECTORY}/token";
  };
};
```

On the host: `settings.tokenFile = config.sops.secrets.web-token.path;`.

## Store paths from the host

A settings string like `/nix/store/…-cert.pem` carries no dependency.
flakelet gc-roots such paths per generation, so referencing them at
runtime is fine. If the build itself must read the file, wrap it:

```nix
impl = { options, inputs }: let inherit (inputs.flakelet) storePath; in {
  services.web.serviceConfig.ExecStart =
    "… --ca ${storePath options.caBundle}";
};
```
