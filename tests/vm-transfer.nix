# export on node a, import on node b: DynamicUser= state, dump/restore
# units and a provider with state hooks.
{ flakeletModule }:
{ pkgs, lib, ... }:
let
  testService = pkgs.writeTextDir "flake.nix" ''
    {
      outputs = _: {
        flakelets.default =
          { types, ... }:
          {
            impl =
              { pkgs, name, ... }:
              {
                services.''${name} = {
                  wantedBy = [ "multi-user.target" ];
                  serviceConfig = {
                    ExecStart = "''${pkgs.coreutils}/bin/sleep infinity";
                    DynamicUser = true;
                    StateDirectory = name;
                  };
                };
                dumpScript = "''${pkgs.bash}/bin/sh -c 'echo dumped > /var/lib/''${name}/dump'";
                restoreScript = "''${pkgs.bash}/bin/sh -c 'test -f /var/lib/''${name}/dump && ''${pkgs.coreutils}/bin/touch /var/lib/''${name}/restored'";
                exports.requires.kv = { bucket = name; };
              };
          };
      };
    }
  '';
  # Stand-in provider: its "resource" is a file under /srv/kv/<bucket>.
  kvProvider = pkgs.writeShellScriptBin "kv" ''
    set -eu
    bucket=$(${pkgs.jq}/bin/jq -r .bucket "$2")
    case "$1" in
      dump) cp /srv/kv/"$bucket" "$3/data" ;;
      restore)
        if [ -z "''${FLAKELET_REPLACE:-}" ]; then test ! -e /srv/kv/"$bucket"; fi
        mkdir -p /srv/kv; cp "$3/data" /srv/kv/"$bucket" ;;
    esac
  '';
  node =
    { config, options, ... }:
    {
      imports = [ flakeletModule ];
      services.flakelets = {
        enable = true;
        services.web.flake = "path:${testService}";
      };
      environment.etc."flakelet/providers.d/kv-v1.json".text = builtins.toJSON {
        contract = "kv/v1";
        state = {
          dump = pkgs.writeShellScript "kv-dump" ''exec ${kvProvider}/bin/kv dump "$@"'';
          restore = pkgs.writeShellScript "kv-restore" ''exec ${kvProvider}/bin/kv restore "$@"'';
        };
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
          pkgs.stdenvNoCC
          pkgs.bash
          pkgs.coreutils
        ];
        memorySize = 4096;
        cores = 4;
      }
      // lib.optionalAttrs (options.virtualisation ? virtiofs) { virtiofs.enable = true; };
    };
in
{
  name = "flakelet-transfer";
  nodes.a = node;
  nodes.b = node;

  testScript = ''
    start_all()
    a.succeed("mkdir -p /srv/kv && echo bucket-data > /srv/kv/web")
    a.succeed("systemctl start flakelet-web.service", timeout=600)
    a.succeed("systemctl is-active web.service")
    a.succeed("echo payload > /var/lib/private/web/file")

    a.succeed("flakelet export web --dry-run | grep -q /var/lib/web")
    a.succeed("flakelet export web --copy > /tmp/shared/web.tar.zst")
    a.succeed("systemctl is-active web.service")
    a.succeed("test -f /var/lib/private/web/dump")

    # A move leaves the source disabled across updates and boot.
    a.succeed("flakelet export web --to b > /tmp/shared/web.tar.zst")
    print(a.succeed("tar --zstd -tf /tmp/shared/web.tar.zst"))
    a.fail("systemctl is-active web.service")
    a.succeed("flakelet update web --no-refresh | grep disabled")
    a.succeed("systemctl restart flakelet-web.service")
    a.succeed("flakelet boot")
    a.fail("systemctl is-active web.service")
    a.succeed("flakelet status web | grep 'exported to b'")
    a.fail("flakelet rollback web")
    a.succeed("flakelet enable web")
    a.succeed("systemctl is-active web.service")
    a.succeed("grep -q payload /var/lib/private/web/file")
    a.succeed("flakelet disable web -m done")


    # Occupy a dynamic uid so b allocates a different one than a.
    b.succeed("systemd-run -p DynamicUser=yes -u squat sleep infinity")
    # A failing restore hook after extraction leaves b empty and disabled.
    b.succeed("systemctl start flakelet-web.service", timeout=600)
    b.succeed("systemctl is-active web.service")
    b.succeed("mkdir -p /srv/kv && echo stale > /srv/kv/web")
    b.fail("flakelet import - --no-refresh < /tmp/shared/web.tar.zst")
    b.fail("systemctl is-active web.service")
    b.succeed("flakelet status web | grep 'did not finish'")
    b.succeed("test -z \"$(ls -A /var/lib/private/web)\"")
    b.succeed("echo junk > /var/lib/private/web/junk")
    b.fail("flakelet import /tmp/shared/web.tar.zst --no-refresh")
    b.succeed("flakelet import /tmp/shared/web.tar.zst --no-refresh --replace", timeout=600)
    b.succeed("test ! -e /var/lib/private/web/junk")
    b.succeed("systemctl is-active web.service")
    b.succeed("flakelet status web | grep ok | grep pinned")
    b.succeed("grep -q payload /var/lib/private/web/file")
    b.succeed("test -f /var/lib/private/web/restored")
    b.succeed("grep -q bucket-data /srv/kv/web")
    # The service's dynamic user (not root) owns the restored files from its
    # own point of view, whether systemd chowned or idmapped them.
    b.succeed("systemd-run --wait --pipe -p DynamicUser=yes -p User=web -p StateDirectory=web sh -c 'test -O /var/lib/web/file && echo more >> /var/lib/web/file'")

    # Second import is refused: state folder and provider resource exist.
    b.fail("flakelet import /tmp/shared/web.tar.zst --no-refresh")
  '';
}
