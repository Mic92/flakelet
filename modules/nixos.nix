# NixOS/systemd wiring for flakelet: eval user, directories, boot relink,
# reconcile and per-service update units.
{
  config,
  lib,
  ...
}:
let
  cfg = config.services.flakelets;
  flakelet = lib.getExe cfg.package;
in
{
  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    environment.etc."flakelet/config.json".source = cfg.configFile;

    users.users.flakelet = {
      isSystemUser = true;
      group = "flakelet";
      home = "/var/lib/flakelet";
    };
    users.groups.flakelet = { };

    systemd.tmpfiles.rules = [
      "d /var/lib/flakelet 0750 root root -"
      "d /var/cache/flakelet 0750 flakelet flakelet -"
    ];

    systemd.targets.flakelet = {
      description = "flakelet managed services";
      wantedBy = [ "multi-user.target" ];
    };

    # Re-link the current generations early at boot; no evaluation, no network.
    systemd.services.flakelet-boot = {
      description = "Re-link flakelet services at boot";
      wantedBy = [ "multi-user.target" ];
      before = [ "flakelet.target" ];
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${flakelet} boot";
      };
    };

    # Remove services that vanished from the host configuration; runs before
    # the per-service update units on every configuration switch.
    systemd.services.flakelet-reconcile = {
      description = "Reconcile flakelet services with the host configuration";
      wantedBy = [ "multi-user.target" ];
      after = [ "flakelet-boot.service" ];
      restartTriggers = [ cfg.configFile ];
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${flakelet} reconcile";
      };
    };

    systemd.services = lib.mapAttrs' (
      name: svc:
      lib.nameValuePair "flakelet-${name}" {
        description = "Update flakelet service ${name}";
        wantedBy = [ "multi-user.target" ];
        wants = [ "network-online.target" ];
        after = [
          "network-online.target"
          "flakelet-reconcile.service"
        ];
        before = [ "flakelet.target" ];
        # Restart (and thereby update) when the service definition changes.
        restartTriggers = [ (builtins.hashString "sha256" (builtins.toJSON svc)) ];
        serviceConfig = {
          Type = "oneshot";
          Nice = 10;
          IOSchedulingClass = "idle";
          MemoryHigh = "75%";
          ExecStart = "${flakelet} update --offline-fallback ${name}";
        };
      }
    ) cfg.services;

    systemd.timers = lib.mapAttrs' (
      name: svc:
      lib.nameValuePair "flakelet-${name}" {
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnCalendar = svc.autoUpdate.interval;
          Persistent = true;
          RandomizedDelaySec = "1h";
        };
      }
    ) (lib.filterAttrs (_: svc: svc.autoUpdate.enable) cfg.services);
  };
}
