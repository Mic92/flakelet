# flakelet.lib, instantiated per service entry by the driver expression and
# injected into service modules as the `flakeletLib` argument.
#
#   flakelets.default = { pkgs, flakeletLib, name, settings, ... }:
#     flakeletLib.mkService {
#       services.${name} = {
#         description = "My service";
#         wantedBy = [ "multi-user.target" ];
#         serviceConfig.ExecStart = "${pkgs.myservice}/bin/serve";
#       };
#     };
{
  pkgs,
  name,
  adios,
}:
let
  lib = pkgs.lib;
  # korora, as vendored by adios; the *Config sections stay freeform attrsets.
  t = import "${adios}/types/types.nix";

  scalar = t.union [
    t.string
    t.number
    t.bool
    t.path
    t.derivation
  ];
  value = t.union [
    scalar
    (t.listOf scalar)
  ];
  strings = t.listOf t.string;
  configSection = t.attrsOf value;

  # NixOS-named dependency options -> [Unit] keys.
  unitKeys = {
    after = "After";
    before = "Before";
    wants = "Wants";
    requires = "Requires";
    requisite = "Requisite";
    bindsTo = "BindsTo";
    partOf = "PartOf";
    conflicts = "Conflicts";
    onFailure = "OnFailure";
  };

  # All members optional, unknown keys hard errors (a typo like
  # `serviceconfig` must not produce a unit without an ExecStart).
  unitType =
    kind: extra:
    (t.struct "mkService ${kind}" (
      lib.mapAttrs (_: _: strings) unitKeys
      // {
        description = t.string;
        documentation = strings;
        wantedBy = strings;
        requiredBy = strings;
        unitConfig = configSection;
      }
      // extra
    )).override
      {
        total = false;
        unknown = false;
      };
  argsType =
    (t.struct "mkService" {
      services = t.attrsOf (unitType "service" {
        serviceConfig = configSection;
        environment = t.attrsOf scalar;
        path = t.listOf (t.union [
          t.derivation
          t.string
          t.path
        ]);
      });
      sockets = t.attrsOf (unitType "socket" { socketConfig = configSection; });
      timers = t.attrsOf (unitType "timer" { timerConfig = configSection; });
      targets = t.attrsOf (unitType "target" { });
      paths = t.attrsOf (unitType "path" { pathConfig = configSection; });
      units = t.attrsOf t.derivation;
      healthCheck = t.union [
        t.derivation
        t.string
      ];
      exports = t.any;
    }).override
      {
        total = false;
        unknown = false;
      };
  check =
    type: v:
    let
      err = type.verify v;
    in
    if err == null then v else throw "flakeletLib.mkService: ${err}";


  # mkValueStringDefault aborts on attrsets, so derivations (e.g. a package
  # as ExecStart) are stringified explicitly.
  toValue = v: if lib.isDerivation v then toString v else lib.generators.mkValueStringDefault { } v;
  toINI = lib.generators.toINI {
    listsAsDuplicateKeys = true;
    mkKeyValue = lib.generators.mkKeyValueDefault { mkValueString = toValue; } "=";
  };
  section = header: attrs: lib.optionalString (attrs != { }) (toINI { ${header} = attrs; });

  unitSection =
    def:
    lib.optionalAttrs (def ? description) { Description = def.description; }
    // lib.optionalAttrs (def ? documentation) { Documentation = def.documentation; }
    // lib.concatMapAttrs (opt: key: lib.optionalAttrs (def ? ${opt}) { ${key} = def.${opt}; }) unitKeys
    // (def.unitConfig or { });

  installSection =
    def:
    lib.optionalAttrs (def ? wantedBy) { WantedBy = def.wantedBy; }
    // lib.optionalAttrs (def ? requiredBy) { RequiredBy = def.requiredBy; };

  # `services.<name>` keeps the entry name, everything else is prefixed, which
  # makes the flakelet multi-instance capable and satisfies flakelet's rule
  # that unit names start with the entry name.
  unitName = key: if key == name then name else "${name}-${key}";

  renderGroup =
    suffix: extraSections: defs:
    lib.mapAttrs' (
      key: def:
      let
        file = "${unitName key}${suffix}";
      in
      lib.nameValuePair file (
        pkgs.writeText file (
          section "Unit" (unitSection def) + extraSections def + section "Install" (installSection def)
        )
      )
    ) defs;

  serviceSection =
    def:
    let
      env =
        lib.optionalAttrs (def.path or [ ] != [ ]) { PATH = lib.makeBinPath def.path; }
        // (def.environment or { });
    in
    section "Service" (
      (def.serviceConfig or { })
      // lib.optionalAttrs (env != { }) {
        # toJSON escapes quotes and backslashes in values.
        Environment = lib.mapAttrsToList (k: v: builtins.toJSON "${k}=${toValue v}") env;
      }
    );
in
{
  mkService =
    args:
    let
      a = check argsType args;
      # Sugar for the `<name>-health.service` probe contract; define
      # services.health directly for full control.
      services =
        (a.services or { })
        // lib.optionalAttrs (a ? healthCheck) {
          health =
            if a ? services.health then
              throw "flakeletLib.mkService: healthCheck and services.health are mutually exclusive"
            else
              {
                description = "health probe for ${name}";
                serviceConfig = {
                  Type = "oneshot";
                  ExecStart = a.healthCheck;
                  DynamicUser = true;
                  TimeoutStartSec = "1min";
                };
              };
        };
      units =
        renderGroup ".service" serviceSection services
        // renderGroup ".socket" (def: section "Socket" (def.socketConfig or { })) (a.sockets or { })
        // renderGroup ".timer" (def: section "Timer" (def.timerConfig or { })) (a.timers or { })
        // renderGroup ".target" (_: "") (a.targets or { })
        // renderGroup ".path" (def: section "Path" (def.pathConfig or { })) (a.paths or { })
        # Raw escape hatch: pre-rendered unit files win over typed ones.
        // (a.units or { });
    in
    if units == { } then
      throw "flakeletLib.mkService: no units defined"
    else
      { inherit units; } // lib.optionalAttrs (a ? exports) { inherit (a) exports; };

  # Turn a bare store path string from the settings into a string with context,
  # so the built artifact really depends on it. builtins.storePath is banned in
  # pure evaluation; appendContext is the pure-mode equivalent.
  storePath =
    p:
    let
      s = builtins.unsafeDiscardStringContext (toString p);
    in
    if !lib.hasPrefix "${builtins.storeDir}/" s then
      throw "flakeletLib.storePath: ${s} is not a store path"
    else
      builtins.appendContext s { ${s}.path = true; };
}
