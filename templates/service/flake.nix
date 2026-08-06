{
  description = "A flakelet service: evaluated and run by flakelet on the target machine";

  outputs = _: {
    # `name` is the entry name from the host configuration; deriving unit and
    # state directory names from it keeps the service multi-instance capable.
    flakelets.default =
      {
        pkgs,
        name,
        settings,
        ...
      }:
      {
        units."${name}.service" = pkgs.writeText "${name}.service" ''
          [Unit]
          Description=${name} example service

          [Service]
          ExecStart=${pkgs.python3}/bin/python3 -m http.server ${toString (settings.port or 8000)}
          DynamicUser=true
          StateDirectory=${name}
          ProtectSystem=strict
          PrivateTmp=true

          [Install]
          WantedBy=multi-user.target
        '';

        # Optional: metadata for firewall/proxy/monitoring/backup integrations,
        # published to /run/flakelet/exports/<name>.json on the host.
        exports.ports.http.port = settings.port or 8000;

        # Optional: executable derivation run after activation; a non-zero exit
        # rolls the service back to the previous generation.
        healthCheck = pkgs.writeShellScript "${name}-health" ''
          exec ${pkgs.curl}/bin/curl -sf http://127.0.0.1:${toString (settings.port or 8000)}/ > /dev/null
        '';
      };
  };
}
