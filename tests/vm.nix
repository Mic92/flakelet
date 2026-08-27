# End-to-end test: a declarative flakelet service is evaluated, built and
# started at runtime inside the VM, without network access.
{ flakeletModule, buildArtifact }:
{ pkgs, lib, ... }:
let
  testService = pkgs.writeTextDir "flake.nix" ''
    {
      outputs = _: {
        flakelets.default =
          { types, ... }:
          {
            options = {
              greeting = { type = types.string; };
              port = {
                type = types.number;
                default = 8080;
                description = "export-only demo port";
              };
            };
            impl =
              { options, pkgs, name, ... }:
              {
                services.''${name} = {
                  description = "flakelet test service";
                  wantedBy = [ "multi-user.target" ];
                  environment.GREETING = options.greeting;
                  serviceConfig.ExecStart = "''${pkgs.coreutils}/bin/sleep infinity";
                };
                exports = {
                  metrics = [ { port = 9100; } ];
                  ports.web.port = options.port;
                };
              };
          };
      };
    }
  '';
  # A prebuilt service artifact, as CI would produce it: no runtime evaluation.
  prebuiltArtifact =
    name:
    buildArtifact pkgs {
      inherit name;
      module =
        { ... }:
        {
          impl =
            { pkgs, name, ... }:
            {
              services.${name} = {
                description = "prebuilt flakelet service ${name}";
                wantedBy = [ "multi-user.target" ];
                serviceConfig.ExecStart = "${pkgs.coreutils}/bin/sleep infinity";
              };
            };
        };
    };
  cliArtifact = prebuiltArtifact "cli";
in
{
  name = "flakelet";

  nodes.machine =
    { config, options, ... }:
    {
      imports = [ flakeletModule ];

      services.flakelets = {
        enable = true;
        services.web = {
          flake = "path:${testService}";
          settings = {
            greeting = "hello";
            port = 8080;
          };
        };
        services.static.prebuilt = prebuiltArtifact "static";
      };

      nix.settings = {
        experimental-features = [
          "nix-command"
          "flakes"
        ];
        substituters = lib.mkForce [ ];
      };

      virtualisation = {
        writableStore = true;
        additionalPaths = [
          config.services.flakelets.nixpkgs
          testService
          # Build-time closure of the runtime-built units (writeText/linkFarm),
          # so the offline VM does not fall back to a source bootstrap.
          pkgs.stdenvNoCC
          pkgs.bash
          pkgs.coreutils
          cliArtifact
        ];
        memorySize = 4096;
        cores = 4;
      }
      # virtiofs instead of 9p for the host store is far faster but not yet
      # in upstream nixpkgs.
      // lib.optionalAttrs (options.virtualisation ? virtiofs) { virtiofs.enable = true; };
    };

  testScript = ''
    machine.wait_for_unit("multi-user.target")

    # The update oneshot evaluates and builds the service at runtime; `start`
    # blocks until it finished and fails loudly if it failed.
    machine.succeed("systemctl start flakelet-web.service", timeout=600)
    print(machine.execute("journalctl -u flakelet-web.service")[1])
    machine.succeed("systemctl is-active web.service")

    # Settings reached the unit, generation and state exist.
    machine.succeed("systemctl show web.service -p Environment | grep -q GREETING=hello")
    machine.succeed("test -f /nix/var/nix/gcroots/flakelet/web/gen-1/manifest.json")
    machine.succeed("flakelet status | grep -q '^web'")

    # Exports of the active generation are published for external consumers.
    machine.succeed("grep -q '\"port\": 8080' /run/flakelet/exports/web.json")

    # A prebuilt artifact is activated without any evaluation.
    machine.succeed("systemctl start flakelet-static.service", timeout=120)
    machine.succeed("systemctl is-active static.service")

    # Imperative activation of a prebuilt artifact via the CLI.
    machine.succeed("flakelet activate cli ${cliArtifact}")
    machine.succeed("systemctl is-active cli.service")
    machine.succeed("flakelet status --json | grep -q 'prebuilt:cli'")

    # Reconcile keeps still-declared and imperative services.
    machine.succeed("flakelet reconcile")
    machine.succeed("systemctl is-active web.service static.service cli.service")

    # Off-machine style check: evaluate and build without touching state,
    # rooting the results for a later deploy step.
    # The directory must be reachable by the unprivileged eval user.
    machine.succeed("flakelet check --build --no-refresh --gc-roots-dir /tmp/roots | grep -q '^web: built /nix/store/'")
    machine.succeed("test -L /tmp/roots/web")
    machine.succeed("flakelet build web --no-refresh --out-link /tmp/out && test -L /tmp/out/web")
    machine.fail("flakelet check nosuchservice --no-refresh")

    # Boot relink restores a lost unit link.
    machine.succeed("rm /run/systemd/system/web.service")
    machine.succeed("flakelet boot")
    machine.succeed("test -L /run/systemd/system/web.service")
  '';
}
