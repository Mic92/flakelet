{
  config,
  lib,
  pkgs,
  ...
}:
# Deploy systemd portable services from flakes, evaluated at runtime by the
# `flakelet` CLI. See PLAN.md next to this file for the design.
let
  cfg = config.services.flakelets;
  settingsFormat = pkgs.formats.json { };

  settingsFile =
    name: svc:
    if svc.settings == { } then
      null
    else
      settingsFormat.generate "flakelet-${name}-settings.json" svc.settings;

  configFile = settingsFormat.generate "flakelet-config.json" {
    eval_user = "flakelet";
    services = lib.mapAttrs (name: svc: {
      inherit (svc) flake profile;
      output = svc.output;
      settings_file = settingsFile name svc;
      extra_portablectl_args = svc.extraPortablectlArgs;
      input_overrides = svc.inputOverrides;
      health_check = {
        timeout = svc.healthCheck.timeout;
        command = svc.healthCheck.command;
      };
      keep_generations = svc.keepGenerations;
    }) cfg.services;
  };

  serviceModule =
    { ... }:
    {
      options = {
        flake = lib.mkOption {
          type = lib.types.str;
          example = "github:Mic92/my-service";
          description = "Flake reference, resolved at runtime on the machine.";
        };
        output = lib.mkOption {
          type = lib.types.str;
          default = "portableServices.${pkgs.stdenv.hostPlatform.system}.default";
          defaultText = "portableServices.\${system}.default";
          description = "Flake output holding the portable service function.";
        };
        settings = lib.mkOption {
          type = settingsFormat.type;
          default = { };
          description = "Host settings passed to the flake output function as JSON.";
        };
        profile = lib.mkOption {
          type = lib.types.str;
          default = "default";
          description = "portablectl profile.";
        };
        extraPortablectlArgs = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [ ];
        };
        inputOverrides = lib.mkOption {
          type = lib.types.attrsOf lib.types.str;
          default = { };
          example = {
            nixpkgs = "github:NixOS/nixpkgs/nixos-25.05";
          };
          description = "Flake inputs to override at evaluation time.";
        };
        autoUpdate = {
          enable = lib.mkEnableOption "periodic re-evaluation of the flake reference";
          interval = lib.mkOption {
            type = lib.types.str;
            default = "daily";
            description = "systemd calendar expression.";
          };
        };
        healthCheck = {
          timeout = lib.mkOption {
            type = lib.types.ints.unsigned;
            default = 30;
            description = "Seconds to wait after attaching before checking unit state.";
          };
          command = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Extra health check command; non-zero exit triggers rollback.";
          };
        };
        keepGenerations = lib.mkOption {
          type = lib.types.ints.positive;
          default = 5;
          description = "Number of generations kept for rollback.";
        };
      };
    };
in
{
  options.services.flakelets = {
    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./. { };
      defaultText = "flakelet";
      description = "flakelet package.";
    };
    services = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule serviceModule);
      default = { };
      description = "Portable services deployed from flakes at runtime.";
    };
  };

  config = lib.mkIf (cfg.services != { }) {
    environment.systemPackages = [ cfg.package ];
    environment.etc."flakelet/config.json".source = configFile;

    users.users.flakelet = {
      isSystemUser = true;
      group = "flakelet";
      home = "/var/lib/flakelet";
    };
    users.groups.flakelet = { };

    systemd.tmpfiles.rules = [
      "d /var/lib/portables 0755 root root -"
      "d /var/lib/flakelet 0755 root root -"
      "d /var/cache/flakelet 0755 flakelet flakelet -"
    ];

    systemd.services = lib.mapAttrs' (
      name: svc:
      lib.nameValuePair "flakelet-${name}" {
        description = "Deploy portable service '${name}' from ${svc.flake}";
        wantedBy = [ "multi-user.target" ];
        after = [
          "network-online.target"
          "nix-daemon.service"
          "systemd-portabled.service"
        ];
        wants = [ "network-online.target" ];
        restartTriggers = [ configFile ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${lib.getExe cfg.package} update --offline-fallback ${lib.escapeShellArg name}";
        };
      }
    ) cfg.services
    // {
      # Remove declarative services that vanished from the configuration.
      flakelet-reconcile = {
        description = "Remove flakelet services no longer in the host configuration";
        wantedBy = [ "multi-user.target" ];
        after = [ "systemd-portabled.service" ];
        restartTriggers = [ configFile ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${lib.getExe cfg.package} reconcile";
        };
      };
    };

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
