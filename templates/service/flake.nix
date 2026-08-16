{
  description = "A flakelet service: evaluated and run by flakelet on the target machine";

  outputs = _: {
    flakelets.default =
      { types, ... }:
      {
        options.port = {
          type = types.number;
          default = 8000;
          description = "TCP port the HTTP server listens on.";
        };

        impl =
          {
            options,
            pkgs,
            name,
            ...
          }:
          {
            services.${name} = {
              description = "${name} example service";
              wantedBy = [ "multi-user.target" ];
              serviceConfig = {
                ExecStart = "${pkgs.python3}/bin/python3 -m http.server ${toString options.port}";
                DynamicUser = true;
                StateDirectory = name;
                ProtectSystem = "strict";
                PrivateTmp = true;
              };
            };

            # A failing health probe rolls the activation back.
            healthCheck = pkgs.writeShellScript "${name}-health" ''
              exec ${pkgs.curl}/bin/curl -sf --retry 5 --retry-connrefused --retry-delay 2 \
                http://127.0.0.1:${toString options.port}/ -o /dev/null
            '';

            # Published to /run/flakelet/exports/<name>.json on the host.
            exports.ports.http.port = options.port;
          };
      };
  };
}
