# flakelet.lib, instantiated per service entry and consumed by the driver
# expression. Service modules follow the adios module shape:
#
#   flakelets.default = { types, ... }: {
#     options = {
#       port = { type = types.number; default = 8000; description = "..."; };
#     };
#     impl = { options, pkgs, name, ... }: {
#       services.${name} = {
#         description = "My service";
#         wantedBy = [ "multi-user.target" ];
#         serviceConfig.ExecStart = "${pkgs.myservice}/bin/serve";
#       };
#     };
#   };
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
  # Exports end up in builtins.toJSON; reject unserializable values early.
  jsonValue = t.typedef' "jsonValue" (
    v:
    if v == null || lib.isString v || lib.isBool v || builtins.isInt v || builtins.isFloat v then
      null
    else if lib.isDerivation v then
      null
    else if lib.isList v then
      lib.foldl' (acc: x: if acc != null then acc else jsonValue.verify x) null v
    else if lib.isAttrs v then
      lib.foldl' (acc: x: if acc != null then acc else jsonValue.verify x) null (
        lib.attrValues v
      )
    else
      "in exports: value of type '${builtins.typeOf v}' is not JSON-serializable"
  );
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
    (t.struct "module ${kind}" (
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
    (t.struct "module" {
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
      exports = t.attrsOf jsonValue;
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
    if err == null then v else fail "in module: ${err}";


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
  adiosTypes = import "${adios}/adios/types.nix" { korora = t; };

  fail = msg: throw "flakelet ${name}: ${msg}";
in
rec {
  types = t;

  # Adios option declarations `{ type, default?, description?, example? }`.
  # Unknown keys are rejected; missing keys take the default, or null when
  # the type is nullable.
  evalOptions =
    options: settings:
    let
      unknown = lib.filter (k: !(options ? ${k})) (lib.attrNames settings);
      checkDecl =
        n: decl:
        let
          e = adiosTypes.modules.option.verify decl;
        in
        if decl ? defaultFunc then
          fail "in option '${n}': defaultFunc is not supported, use default"
        else if e != null then
          fail "in option '${n}': ${e}"
        else
          decl;
      value =
        n: decl:
        if settings ? ${n} then
          let
            e = decl.type.verify settings.${n};
          in
          if e != null then fail "in setting '${n}': ${e}" else settings.${n}
        else if decl ? default then
          decl.default
        else if decl.type.verify null == null then
          null
        else
          fail "missing required setting '${n}'";
    in
    if unknown != [ ] then
      fail "unknown setting(s) ${lib.concatStringsSep ", " unknown}; known: ${lib.concatStringsSep ", " (lib.attrNames options)}"
    else
      lib.mapAttrs (n: decl: value n (checkDecl n decl)) options;

  evalModule =
    def:
    {
      settings,
      extraModules ? [ ],
    }:
    let
      m =
        if lib.isFunction def then
          def { types = t; }
        else
          fail "the module must be a function: { types, ... }: { options, impl }";
    in
    if m ? inputs then
      fail "module inputs are not supported; pkgs, name and helpers are injected into impl"
    else
      render (m.impl {
        options = evalOptions (m.options or { }) settings;
        inherit
          pkgs
          name
          contracts
          storePath
          extraModules
          ;
      });

  # Validate the attrset impl returns and render it into unit files.
  render =
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
              fail "healthCheck and services.health are mutually exclusive"
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
      fail "no units defined"
    else
      { inherit units; } // lib.optionalAttrs (a ? exports) { inherit (a) exports; };

  # Blessed contract constructors; the JSON shape is the interface
  # (contracts/*.json), these only check it at evaluation time.
  contracts.http =
    args:
    let
      v = {
        paths = [ "/" ];
        websockets = false;
        maxBodySize = "1m";
        readTimeout = "60s";
        buffering = true;
        extra = { };
      }
      // args;
      type =
        (t.struct "http/v1" {
          host = t.string;
          upstream = t.string;
          paths = t.listOf t.string;
          websockets = t.bool;
          maxBodySize = t.string;
          readTimeout = t.string;
          buffering = t.bool;
          # Non-portable escape hatch, keyed by webserver implementation.
          extra = t.attrsOf t.string;
        }).override
          { unknown = false; };
      err = type.verify v;
    in
    if err == null then v else fail "in contracts.http: ${err}";

  # Turn a bare store path string from the settings into a string with context,
  # so the built artifact really depends on it. builtins.storePath is banned in
  # pure evaluation; appendContext is the pure-mode equivalent.
  storePath =
    p:
    let
      s = builtins.unsafeDiscardStringContext (toString p);
    in
    if !lib.hasPrefix "${builtins.storeDir}/" s then
      fail "storePath: ${s} is not a store path"
    else
      builtins.appendContext s { ${s}.path = true; };
}
