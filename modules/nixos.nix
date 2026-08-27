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
  updateService = name: flags: {
    wants = [ "network-online.target" ];
    after = [
      "network-online.target"
      "flakelet-reconcile.service"
    ];
    serviceConfig = {
      Type = "oneshot";
      Nice = 10;
      IOSchedulingClass = "idle";
      MemoryHigh = "75%";
      ExecStart = "${flakelet} update --offline-fallback ${flags}${name}";
    };
  };
in
{
  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    environment.etc."flakelet/config.json".source = cfg.configFile;

    users.users.flakelet = {
      isSystemUser = true;
      group = "flakelet";
      home = "/var/cache/flakelet";
    };
    users.groups.flakelet = { };
    # Evaluation and builds run as this user via the daemon.
    nix.settings.extra-allowed-users = [ "flakelet" ];

    systemd.tmpfiles.rules = [
      "d /var/lib/flakelet 0750 root root -"
      "d /var/cache/flakelet 0750 flakelet flakelet -"
    ];

    systemd.targets.flakelet = {
      description = "flakelet managed services";
      wantedBy = [ "multi-user.target" ];
    };

    systemd.services = {
      # Re-link the current generations early at boot; no evaluation, no network.
      flakelet-boot = {
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
      flakelet-reconcile = {
        description = "Reconcile flakelet services with the host configuration";
        wantedBy = [ "multi-user.target" ];
        after = [ "flakelet-boot.service" ];
        restartTriggers = [ cfg.configFile ];
        serviceConfig = {
          Type = "oneshot";
          # Stay active so a switch only re-runs this when the trigger changes.
          RemainAfterExit = true;
          ExecStart = "${flakelet} reconcile";
        };
      };
    }
    # Runs once per definition change (RemainAfterExit + restartTriggers) ...
    // lib.mapAttrs' (
      name: svc:
      lib.nameValuePair "flakelet-${name}" (
        lib.recursiveUpdate (updateService name "") {
          description = "Update flakelet service ${name}";
          wantedBy = [ "multi-user.target" ];
          before = [ "flakelet.target" ];
          restartTriggers = [ (builtins.hashString "sha256" (builtins.toJSON svc)) ];
          serviceConfig.RemainAfterExit = true;
        }
      )
    ) cfg.services
    # ... while the timer needs a unit that goes inactive after each run.
    // lib.mapAttrs' (
      name: _:
      lib.nameValuePair "flakelet-${name}-auto" (
        # Timer ticks must not queue up behind a running update.
        updateService name "--no-wait " // { description = "Scheduled update of flakelet service ${name}"; }
      )
    ) (lib.filterAttrs (_: svc: svc.autoUpdate.enable) cfg.services);

    systemd.timers = lib.mapAttrs' (
      name: svc:
      lib.nameValuePair "flakelet-${name}-auto" {
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnCalendar = svc.autoUpdate.interval;
          Persistent = true;
          RandomizedDelaySec = svc.autoUpdate.randomizedDelay;
          FixedRandomDelay = true;
        };
      }
    ) (lib.filterAttrs (_: svc: svc.autoUpdate.enable) cfg.services);
  };
}
