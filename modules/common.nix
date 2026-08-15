# Platform-agnostic options and rendering of /etc/flakelet/config.json.
# Unit wiring lives in the platform modules (nixos.nix, later darwin/system-manager).
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.flakelets;
  json = pkgs.formats.json { };

  credentialOption =
    description:
    lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      inherit description;
    };

  serviceModule = {
    options = {
      flake = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "github:Mic92/my-service";
        description = "Flake reference, resolved at runtime on the machine.";
      };
      prebuilt = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = ''
          Store path of an already built service artifact (settings baked in).
          Mutually exclusive with `flake`; nothing is evaluated at runtime.
        '';
      };
      output = lib.mkOption {
        type = lib.types.str;
        default = "flakelets.default";
        description = "Flake output attribute holding the flakelet module.";
      };
      settings = lib.mkOption {
        type = json.type;
        default = { };
        description = "Host settings passed to the flakelet module.";
      };
      inputOverrides = lib.mkOption {
        type = lib.types.attrsOf lib.types.str;
        default = { };
        description = "Flake inputs to override at evaluation time.";
      };
      keepGenerations = lib.mkOption {
        type = lib.types.ints.positive;
        default = 5;
        description = "Number of generations to keep for rollback.";
      };
      autoUpdate = {
        enable = lib.mkEnableOption "periodic re-evaluation of the flake reference";
        interval = lib.mkOption {
          type = lib.types.str;
          default = "daily";
          description = "systemd calendar expression.";
        };
        randomizedDelay = lib.mkOption {
          type = lib.types.str;
          default = "1h";
          description = ''
            Spread timer firings across the fleet, so an update does not
            restart a service on every host at once. Each host keeps a stable
            offset (FixedRandomDelay), which makes rollouts predictable.
          '';
        };
      };
    };
  };
in
{
  options.services.flakelets = {
    enable = lib.mkEnableOption "flakelet, runtime-managed systemd services from Nix flakes";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The flakelet package.";
    };

    nixpkgs = lib.mkOption {
      type = lib.types.path;
      default = pkgs.path;
      defaultText = lib.literalExpression "pkgs.path";
      description = "nixpkgs source imported by the driver expression.";
    };

    adios = lib.mkOption {
      type = lib.types.path;
      description = "adios library source injected into service modules.";
    };

    flakeletLib = lib.mkOption {
      type = lib.types.path;
      description = "flakelet.lib source (mkService, storePath), injected into service modules.";
    };

    extraModules = lib.mkOption {
      type = lib.types.listOf lib.types.path;
      default = [ ];
      description = "Host-provided helper modules passed to service functions.";
    };

    eval = {
      workers = lib.mkOption {
        type = lib.types.ints.positive;
        default = 1;
        description = "nix-eval-jobs worker count.";
      };
      maxMemoryMb = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.positive;
        default = null;
        description = "Restart an eval worker above this many MiB (default: derived from RAM).";
      };
    };

    credentials = {
      netrcFile = credentialOption "netrc file for https flake fetches (path to a secret on the host).";
      accessTokensFile = credentialOption "File with `host=token` lines for nix access-tokens.";
      sshKeyFile = credentialOption "SSH private key for git+ssh flake fetches.";
      sshKnownHostsFile = credentialOption "known_hosts file used for git+ssh flake fetches.";
    };

    services = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule serviceModule);
      default = { };
      description = "Declarative flakelet services.";
    };

    configFile = lib.mkOption {
      type = lib.types.package;
      readOnly = true;
      internal = true;
      description = "Rendered /etc/flakelet/config.json.";
    };
  };

  config = lib.mkIf cfg.enable {
    services.flakelets.configFile = json.generate "flakelet-config.json" {
      version = 1;
      eval_user = "flakelet";
      nixpkgs = "${cfg.nixpkgs}";
      adios = "${cfg.adios}";
      flakelet_lib = "${cfg.flakeletLib}";
      extra_modules = map toString cfg.extraModules;
      eval = {
        workers = cfg.eval.workers;
        max_memory_mb = cfg.eval.maxMemoryMb;
      };
      credentials = {
        netrc_file = cfg.credentials.netrcFile;
        access_tokens_file = cfg.credentials.accessTokensFile;
        ssh_key_file = cfg.credentials.sshKeyFile;
        ssh_known_hosts_file = cfg.credentials.sshKnownHostsFile;
      };
      services = lib.mapAttrs (
        _: svc:
        lib.filterAttrs (_: v: v != null) {
          inherit (svc)
            flake
            output
            settings
            prebuilt
            ;
          input_overrides = svc.inputOverrides;
          keep_generations = svc.keepGenerations;
        }
      ) cfg.services;
    };

    assertions = lib.mapAttrsToList (name: svc: {
      assertion = (svc.flake == null) != (svc.prebuilt == null);
      message = "services.flakelets.services.${name}: set exactly one of `flake` or `prebuilt`";
    }) cfg.services;
  };
}
