# End-to-end test: a declarative flakelet service is evaluated, built and
# started at runtime inside the VM, without network access.
{ flakeletModule }:
{ pkgs, lib, ... }:
let
  testService = pkgs.writeTextDir "flake.nix" ''
    {
      outputs = _: {
        flakelets.default =
          { pkgs, name, settings, ... }:
          {
            units."''${name}.service" = pkgs.writeText "''${name}.service" '''
              [Unit]
              Description=flakelet test service
              [Service]
              Environment=GREETING=''${settings.greeting}
              ExecStart=''${pkgs.coreutils}/bin/sleep infinity
              [Install]
              WantedBy=multi-user.target
            ''';
          };
      };
    }
  '';
  # A prebuilt service artifact, as CI would produce it: no runtime evaluation.
  prebuiltArtifact =
    name:
    pkgs.linkFarm "flakelet-${name}" {
      "units/${name}.service" = pkgs.writeText "${name}.service" ''
        [Unit]
        Description=prebuilt flakelet service ${name}
        [Service]
        ExecStart=${pkgs.coreutils}/bin/sleep infinity
        [Install]
        WantedBy=multi-user.target
      '';
      "meta.json" = pkgs.writeText "meta.json" (builtins.toJSON { flake_url = "prebuilt:${name}"; });
    };
  cliArtifact = prebuiltArtifact "cli";
in
{
  name = "flakelet";

  nodes.machine =
    { config, ... }:
    {
      imports = [ flakeletModule ];

      services.flakelets = {
        enable = true;
        adios = lib.mkForce null; # the test service does not use it
        services.web = {
          flake = "path:${testService}";
          settings.greeting = "hello";
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
        # virtiofs instead of 9p for the host store: evaluating nixpkgs at
        # runtime is far faster and nothing needs to be copied into an image.
        virtiofs.enable = true;
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
      };
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

    # Boot relink restores a lost unit link.
    machine.succeed("rm /run/systemd/system/web.service")
    machine.succeed("flakelet boot")
    machine.succeed("test -L /run/systemd/system/web.service")
  '';
}
