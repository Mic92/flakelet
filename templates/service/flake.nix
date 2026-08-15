{
  description = "A flakelet service: evaluated and run by flakelet on the target machine";

  outputs = _: {
    # `name` is the entry name from the host configuration; flakeletLib derives
    # unit and state directory names from it, which keeps the service
    # multi-instance capable. Everything is injected by the host: no inputs.
    flakelets.default =
      {
        pkgs,
        flakeletLib,
        name,
        settings,
        ...
      }:
      let
        port = toString (settings.port or 8000);
      in
      flakeletLib.mkService {
        services.${name} = {
          description = "${name} example service";
          wantedBy = [ "multi-user.target" ];
          serviceConfig = {
            ExecStart = "${pkgs.python3}/bin/python3 -m http.server ${port}";
            DynamicUser = true;
            StateDirectory = name;
            ProtectSystem = "strict";
            PrivateTmp = true;
          };
        };

        # Optional health probe: started after every activation, a failure
        # rolls the service back.
        healthCheck = pkgs.writeShellScript "${name}-health" ''
          exec ${pkgs.curl}/bin/curl -sf --retry 5 --retry-connrefused --retry-delay 2 \
            http://127.0.0.1:${port}/ -o /dev/null
        '';

        # Optional: metadata for firewall/proxy/monitoring/backup integrations,
        # published to /run/flakelet/exports/<name>.json on the host.
        exports.ports.http.port = settings.port or 8000;
      };
  };
}
