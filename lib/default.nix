# flakelet.lib, instantiated per service entry and consumed by the driver
# expression. Service modules follow the adios module shape:
#
#   flakelets.default = { types, ... }: {
#     options = {
#       port = { type = types.number; default = 8000; description = "..."; };
#     };
#     impl = { options, inputs }: {
#       services.${inputs.flakelet.name} = {
#         description = "My service";
#         wantedBy = [ "multi-user.target" ];
#         serviceConfig.ExecStart = "${inputs.nixpkgs.pkgs.myservice}/bin/serve";
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
  adiosLib = import "${adios}/adios";
  # The *Config sections stay freeform attrsets.
  t = adiosLib.types;

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
  script = t.union [
    t.derivation
    t.string
  ];
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
      lib.foldl' (acc: x: if acc != null then acc else jsonValue.verify x) null (lib.attrValues v)
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
      services = t.attrsOf (
        unitType "service" {
          serviceConfig = configSection;
          environment = t.attrsOf scalar;
          path = t.listOf (
            t.union [
              t.derivation
              t.string
              t.path
            ]
          );
        }
      );
      sockets = t.attrsOf (unitType "socket" { socketConfig = configSection; });
      timers = t.attrsOf (unitType "timer" { timerConfig = configSection; });
      targets = t.attrsOf (unitType "target" { });
      paths = t.attrsOf (unitType "path" { pathConfig = configSection; });
      healthCheck = script;
      dumpScript = script;
      restoreScript = script;
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
  section =
    header: attrs:
    lib.optionalString (attrs != { }) (toINI {
      ${header} = attrs;
    });

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
      # NixOS' baseline, so ported scripts keep working.
      path = (def.path or [ ]) ++ [
        pkgs.coreutils
        pkgs.findutils
        pkgs.gnugrep
        pkgs.gnused
        pkgs.systemdMinimal
      ];
      env = {
        PATH = "${lib.makeBinPath path}:${lib.makeSearchPathOutput "bin" "sbin" path}";
      }
      // (def.environment or { });
    in
    section "Service" (
      # exec instead of systemd's simple: a missing binary or User= then
      # fails the start job and with it the deploy.
      {
        Type = "exec";
      }
      // (def.serviceConfig or { })
      // {
        # toJSON escapes quotes and backslashes in values.
        Environment =
          lib.mapAttrsToList (k: v: builtins.toJSON "${k}=${toValue v}") env
          ++ lib.toList (def.serviceConfig.Environment or [ ]);
      }
    );

  fail = msg: throw "flakelet ${name}: ${msg}";
  isTrue =
    v:
    lib.elem v [
      true
      "yes"
      "true"
      "on"
      "1"
      1
    ];
in
rec {
  types = t;

  # The service module becomes /<name> in an adios tree next to /nixpkgs
  # and /flakelet, which it gets as inputs unless it declares its own.
  evalModule =
    def:
    {
      settings,
      extraModules ? [ ],
    }:
    let
      m =
        if lib.isFunction def then
          def adiosLib
        else
          fail "the module must be a function: { types, ... }: { options, impl }";
      declared = m.options or { };
      unknown = lib.filter (k: !(declared ? ${k})) (lib.attrNames settings);
      # adios leaves these unset, a service wants an error naming the key.
      missing = lib.filter (
        k:
        let
          o = declared.${k};
        in
        !(settings ? ${k} || o ? default || o ? defaultFunc || o ? options || o.type.verify null == null)
      ) (lib.attrNames declared);
      nullDefaults = lib.mapAttrs (_: _: null) (
        lib.filterAttrs (
          k: o: !(settings ? ${k} || o ? default || o ? defaultFunc || o ? options)
        ) declared
      );
      tree = adiosLib {
        name = "flakelet";
        modules = {
          nixpkgs.options = {
            pkgs.type = t.attrs;
            lib.type = t.attrs;
          };
          flakelet.options = {
            name.type = t.string;
            contracts.type = t.attrs;
            storePath.type = t.function;
            extraModules.type = t.listOf t.attrs;
          };
          ${name} = m // {
            inputs = {
              nixpkgs.path = "/nixpkgs";
              flakelet.path = "/flakelet";
            }
            // (m.inputs or { });
          };
        };
      };
      applied = tree {
        options = {
          "/nixpkgs" = {
            inherit pkgs lib;
          };
          "/flakelet" = {
            inherit
              name
              contracts
              storePath
              extraModules
              ;
          };
          "/${name}" = nullDefaults // settings;
        };
      };
      svc = applied.modules.${name};
    in
    if !(m ? impl) then
      fail "the module has no impl"
    else if unknown != [ ] then
      fail "unknown setting(s) ${lib.concatStringsSep ", " unknown}; the module declares ${
        if declared == { } then "none" else lib.concatStringsSep ", " (lib.attrNames declared)
      }"
    else if missing != [ ] then
      fail "missing required setting(s) ${lib.concatStringsSep ", " missing}"
    else
      # impl may never touch options, the settings must still be checked.
      builtins.deepSeq svc.args.options (render (svc { }));

  # Validate the attrset impl returns and render it into unit files.
  render =
    args:
    let
      a = check argsType args;
      main = a.services.${name}.serviceConfig or { };
      # Sugar units run as the main unit's user so probes reach 0660 sockets
      # and dump/restore reach the state. systemd keys dynamic users by name,
      # so an explicit User= yields the same uid.
      identity =
        lib.optionalAttrs (isTrue (main.DynamicUser or false)) { User = name; }
        // builtins.intersectAttrs {
          User = 1;
          Group = 1;
          DynamicUser = 1;
          StateDirectory = 1;
        } main;
      sugar =
        key: opt:
        {
          needsState ? true,
          extra ? { },
        }:
        if !(a ? ${opt}) then
          { }
        else if a ? services.${key} then
          fail "${opt} and services.${key} are mutually exclusive"
        else if needsState && !(main ? StateDirectory) then
          fail "${opt} needs services.${name} with a StateDirectory= to read and write"
        else
          {
            ${key} = {
              description = "${key} for ${name}";
              serviceConfig = {
                Type = "oneshot";
                ExecStart = a.${opt};
              }
              // identity
              // extra;
            };
          };
      services =
        (a.services or { })
        // sugar "health" "healthCheck" {
          needsState = false;
          extra.TimeoutStartSec = "1min";
        }
        // sugar "dump" "dumpScript" { }
        // sugar "restore" "restoreScript" { };
      units =
        renderGroup ".service" serviceSection services
        // renderGroup ".socket" (def: section "Socket" (def.socketConfig or { })) (a.sockets or { })
        // renderGroup ".timer" (def: section "Timer" (def.timerConfig or { })) (a.timers or { })
        // renderGroup ".target" (_: "") (a.targets or { })
        // renderGroup ".path" (def: section "Path" (def.pathConfig or { })) (a.paths or { });
    in
    if units == { } then
      fail "no units defined"
    else
      {
        inherit units;
        state = deriveState services (a.exports.state or { });
      }
      // lib.optionalAttrs (a ? exports) { inherit (a) exports; };

  deriveState =
    services: declared:
    let
      d = check ((t.struct "exports.state" { extraFolders = t.listOf t.string; }).override {
        total = false;
        unknown = false;
      }) declared;
      extraFolders = d.extraFolders or [ ];
      words =
        v:
        if lib.isList v then
          lib.concatMap words v
        else
          lib.filter (s: s != "") (lib.splitString " " (toString v));
      owner = sc: rec {
        dynamic = isTrue (sc.DynamicUser or false);
        user = sc.User or (if dynamic then name else "root");
        # null: the user's login group, resolved by chown on the target.
        group = sc.Group or null;
      };
      fromUnit =
        key:
        let
          sc = services.${key}.serviceConfig or { };
        in
        # StateDirectory=a:b makes b a symlink to a. Only a holds data.
        map (dir: owner sc // { path = "/var/lib/${lib.head (lib.splitString ":" dir)}"; }) (
          words (sc.StateDirectory or [ ])
        );
      main = services.${name}.serviceConfig or { };
      # Main unit first so a StateDirectory shared with a helper unit is
      # attributed to it. listToAttrs keeps the first occurrence per path.
      keys =
        lib.optional (services ? ${name}) name
        ++ lib.attrNames (
          removeAttrs services [
            name
            "dump"
            "restore"
            "health"
          ]
        );
      folders =
        lib.concatMap fromUnit keys
        ++ map (
          path:
          owner main
          // {
            inherit path;
            dynamic = false;
          }
        ) extraFolders;
    in
    if lib.any (p: !(lib.hasPrefix "/" p) || lib.hasPrefix builtins.storeDir p) extraFolders then
      fail "exports.state.extraFolders must be absolute non-store paths"
    else if extraFolders != [ ] && (owner main).dynamic then
      fail "exports.state.extraFolders needs a static User= on services.${name}"
    else
      {
        # First claim per path wins and order is kept. The archive is positional.
        folders = lib.foldl' (
          acc: f: if lib.any (g: g.path == f.path) acc then acc else acc ++ [ f ]
        ) [ ] folders;
        dump = if services ? dump then "${name}-dump.service" else null;
        restore = if services ? restore then "${name}-restore.service" else null;
      };

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
