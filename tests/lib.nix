# Exercises flakelet.lib: mkService rendering, name prefixing, the raw units
# escape hatch, unknown-key errors and the storePath helper.
{
  pkgs,
  lib,
  runCommand,
  linkFarm,
  writeText,
  hello,
  adios,
}:
let
  flakeletLib = import ../lib {
    inherit pkgs adios;
    name = "web";
  };

  result = flakeletLib.mkService {
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
      };
    };
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
    units."web-raw.service" = writeText "web-raw.service" "[Service]\n";
    healthCheck = "/bin/probe";
    exports.ports.http.port = 8080;
  };

  fails = expr: !(builtins.tryEval (builtins.deepSeq expr expr)).success;

  # storePath turns a context-free store path string into a real dependency.
  helloPath = flakeletLib.storePath (builtins.unsafeDiscardStringContext "${hello}");
in
# Unknown keys, wrong value types and empty results are hard errors.
assert fails (flakeletLib.mkService { bogus = 1; });
assert fails (flakeletLib.mkService { services.web.serviceconfig = { }; });
assert fails (flakeletLib.mkService { services.web.after = "network.target"; });
assert fails (flakeletLib.mkService { services.web.wantedBy = [ 5 ]; });
assert fails (flakeletLib.mkService { services.web.description = [ "x" ]; });
assert fails (flakeletLib.mkService { });
assert fails (flakeletLib.storePath "/etc/passwd");
assert builtins.hasContext helloPath;
assert result.exports.ports.http.port == 8080;
assert lib.attrNames result.units == [
  "web-gc.timer"
  "web-health.service"
  "web-pre.target"
  "web-raw.service"
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
  grep -qx 'Environment="PATH=${lib.makeBinPath [ hello ]}"' $s
  grep -qx 'WantedBy=multi-user.target' $s
  grep -qx 'ListenStream=8080' $units/web.socket
  grep -qx 'OnCalendar=daily' $units/web-gc.timer
  grep -qx 'WantedBy=timers.target' $units/web-gc.timer
  grep -qx 'ExecStart=/bin/false worker' $units/web-worker.service
  grep -qx 'Description=setup done' $units/web-pre.target
  grep -qx 'PathChanged=/var/lib/web' $units/web-watch.path
  grep -qx 'ExecStart=/bin/probe' $units/web-health.service
  grep -qx 'Type=oneshot' $units/web-health.service
  touch $out
''
