# Exercises flakelet.lib: option evaluation, unit rendering, name prefixing,
# derived state, unknown-key errors and the storePath helper.
{
  pkgs,
  lib,
  runCommand,
  linkFarm,
  hello,
  adios,
}:
let
  flakeletLib = import ../lib {
    inherit pkgs adios;
    name = "web";
  };

  checked = flakeletLib.evalOptions {
    greeting = {
      type = flakeletLib.types.string;
      description = "demo greeting";
    };
    port = {
      type = flakeletLib.types.number;
      default = 8080;
    };
    token = {
      type = flakeletLib.types.option flakeletLib.types.string;
    };
  } { greeting = "hi"; };

  evaluated = flakeletLib.evalModule (
    { types, ... }:
    {
      options.port = {
        type = types.number;
        default = 9000;
      };
      impl =
        {
          options,
          pkgs,
          name,
          ...
        }:
        {
          services.${name}.serviceConfig.ExecStart =
            "${pkgs.coreutils}/bin/true --port ${toString options.port}";
          exports.ports.http.port = options.port;
        };
    }
  ) { settings = { }; };

  result = flakeletLib.render {
    services.web = {
      description = "demo web service";
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];
      path = [ hello ];
      environment.FOO = "bar";
      environment.QUOTED = ''va"lue'';
      serviceConfig = {
        ExecStart = "/bin/false --port 80";
        DynamicUser = true;
        StateDirectory = "web web/sub alias:link";
        CacheDirectory = "web";
        ProtectHome = true;
      };
    };
    dumpScript = "/bin/dump";
    # Not called `web` -> prefixed to web-worker.service.
    services.worker.serviceConfig.ExecStart = "/bin/false worker";
    sockets.web = {
      socketConfig.ListenStream = 8080;
      wantedBy = [ "sockets.target" ];
    };
    timers.gc = {
      timerConfig.OnCalendar = "daily";
      wantedBy = [ "timers.target" ];
    };
    targets.pre = {
      description = "setup done";
    };
    paths.watch = {
      pathConfig.PathChanged = "/var/lib/web";
      wantedBy = [ "paths.target" ];
    };
    healthCheck = "/bin/probe";
    exports.ports.http.port = 8080;
  };

  fails = expr: !(builtins.tryEval (builtins.deepSeq expr expr)).success;

  static = flakeletLib.render {
    services.web.serviceConfig = {
      ExecStart = "/bin/false";
      User = "webuser";
      Group = "webgrp";
      StateDirectory = [ "web" ];
    };
    services.worker.serviceConfig = {
      ExecStart = "/bin/false";
      StateDirectory = "web";
    };
    restoreScript = "/bin/restore";
    exports.state.extraFolders = [ "/srv/media" ];
  };

  # storePath turns a context-free store path string into a real dependency.
  helloPath = flakeletLib.storePath (builtins.unsafeDiscardStringContext "${hello}");
in
# Unknown keys, wrong value types and empty results are hard errors.
assert
  flakeletLib.contracts.http {
    host = "x.example.com";
    upstream = "unix:/run/web/web.sock";
    readTimeout = "3600s";
    buffering = false;
  } == {
    host = "x.example.com";
    upstream = "unix:/run/web/web.sock";
    paths = [ "/" ];
    websockets = false;
    maxBodySize = "1m";
    readTimeout = "3600s";
    buffering = false;
    extra = { };
  };
assert fails (flakeletLib.contracts.http { host = "x"; });
assert fails (
  flakeletLib.contracts.http {
    host = "x";
    upstream = "u";
    websokets = true;
  }
);
assert fails (flakeletLib.render { bogus = 1; });
assert fails (flakeletLib.render { services.web.serviceconfig = { }; });
assert fails (flakeletLib.render { services.web.after = "network.target"; });
assert fails (flakeletLib.render { services.web.wantedBy = [ 5 ]; });
assert fails (flakeletLib.render { services.web.description = [ "x" ]; });
assert fails (flakeletLib.render { });
assert fails (flakeletLib.storePath "/etc/passwd");
assert
  checked == {
    greeting = "hi";
    port = 8080;
    token = null;
  };
assert fails (
  flakeletLib.evalOptions {
    p = {
      type = flakeletLib.types.number;
    };
  } { q = 1; }
);
assert fails (
  flakeletLib.evalOptions {
    p = {
      type = flakeletLib.types.number;
    };
  } { p = "x"; }
);
assert fails (
  flakeletLib.evalOptions {
    p = {
      type = flakeletLib.types.number;
    };
  } { }
);
assert fails (
  flakeletLib.evalOptions {
    p = {
      type = flakeletLib.types.number;
      defaultFunc = _: 1;
    };
  } { p = 1; }
);
assert evaluated.exports.ports.http.port == 9000;
assert lib.attrNames evaluated.units == [ "web.service" ];
assert fails (flakeletLib.evalModule { options = { }; } { settings = { }; });
assert fails (
  flakeletLib.evalModule (_: {
    inputs.nixpkgs.path = "/nixpkgs";
    impl = _: { };
  }) { settings = { }; }
);
# Unknown settings are rejected even when impl never reads options.
assert fails (
  (flakeletLib.evalModule (_: {
    impl = _: { services.web.serviceConfig.ExecStart = "/bin/true"; };
  }) { settings.typo = 1; }).units
);
assert builtins.hasContext helloPath;
assert
  result.state == {
    folders =
      map
        (path: {
          inherit path;
          user = "web";
          group = null;
          dynamic = true;
        })
        [
          "/var/lib/alias"
          "/var/lib/web"
          "/var/lib/web/sub"
        ];
    dump = "web-dump.service";
    restore = null;
  };
assert evaluated.state.folders == [ ];
assert
  static.state.folders == map
    (path: {
      inherit path;
      user = "webuser";
      group = "webgrp";
      dynamic = false;
    })
    [
      "/srv/media"
      "/var/lib/web"
    ];
assert static.state.restore == "web-restore.service";
assert fails
  (flakeletLib.render {
    services.web.serviceConfig = {
      ExecStart = "x";
      DynamicUser = true;
    };
    exports.state.extraFolders = [ "/srv/x" ];
  }).state;
assert fails
  (flakeletLib.render {
    services.web.serviceConfig.ExecStart = "x";
    exports.state.extraFolders = [ "relative" ];
  }).state;
assert fails
  (flakeletLib.render {
    services.web.serviceConfig.ExecStart = "x";
    exports.state.bogus = 1;
  }).state;
assert fails (
  flakeletLib.render {
    services.web.serviceConfig.ExecStart = "x";
    services.dump.serviceConfig.ExecStart = "y";
    dumpScript = "z";
  }
);
assert result.exports.ports.http.port == 8080;
assert
  lib.attrNames result.units == [
    "web-dump.service"
    "web-gc.timer"
    "web-health.service"
    "web-pre.target"
    "web-watch.path"
    "web-worker.service"
    "web.service"
    "web.socket"
  ];
assert !(result ? healthCheck);
runCommand "flakelet-lib-test" { units = linkFarm "flakelet-lib-test-units" result.units; } ''
  s=$units/web.service
  grep -qx 'Description=demo web service' $s
  grep -qx 'After=network.target' $s
  grep -qx 'ExecStart=/bin/false --port 80' $s
  grep -qx 'DynamicUser=true' $s
  grep -qx 'Environment="FOO=bar"' $s
  grep -qxF 'Environment="QUOTED=va\"lue"' $s
  grep -q '^Environment="PATH=${lib.makeBinPath [ hello ]}:.*coreutils' $s
  grep -qx 'WantedBy=multi-user.target' $s
  grep -qx 'ListenStream=8080' $units/web.socket
  grep -qx 'OnCalendar=daily' $units/web-gc.timer
  grep -qx 'WantedBy=timers.target' $units/web-gc.timer
  grep -qx 'ExecStart=/bin/false worker' $units/web-worker.service
  grep -qx 'Description=setup done' $units/web-pre.target
  grep -qx 'PathChanged=/var/lib/web' $units/web-watch.path
  grep -qx 'ExecStart=/bin/probe' $units/web-health.service
  grep -qx 'Type=oneshot' $units/web-health.service
  d=$units/web-dump.service
  grep -qx 'ExecStart=/bin/dump' $d
  grep -qx 'Type=oneshot' $d
  grep -qx 'User=web' $d
  grep -qx 'DynamicUser=true' $d
  grep -qx 'StateDirectory=web web/sub alias:link' $d
  if grep -q 'Install' $d; then exit 1; fi
  touch $out
''
