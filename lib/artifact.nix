# Assemble the self-describing service artifact (meta.json, state.json,
# units/, exports.json) from an evaluated module. The on-machine driver
# expression renders the same layout; keep them in sync.
{
  pkgs,
  adios,
  flakeletLib ? ./.,
}:
{
  name,
  # flakelets.<attr> function: { types, ... }: { options; impl; }
  module,
  settings ? { },
  extraModules ? [ ],
  # Provenance shown by `flakelet status`; no evaluation happens on the machine.
  flakeUrl ? "prebuilt:${name}",
  flakeRev ? "",
}:
let
  lib' = import flakeletLib { inherit pkgs name adios; };
  evaluated = lib'.evalModule module { inherit settings extraModules; };
  resolveExports =
    v:
    if pkgs.lib.isDerivation v then
      "${v}"
    else if builtins.isAttrs v then
      builtins.mapAttrs (_: resolveExports) v
    else if builtins.isList v then
      map resolveExports v
    else
      v;
  json = file: value: pkgs.writeText "flakelet-${name}-${file}" (builtins.toJSON value);
in
pkgs.linkFarm "flakelet-${name}" (
  {
    "meta.json" = json "meta.json" {
      version = 1;
      inherit name;
      flake_url = flakeUrl;
      flake_rev = flakeRev;
      settings_hash = builtins.hashString "sha256" (builtins.toJSON settings);
    };
    "state.json" = json "state.json" evaluated.state;
    units = pkgs.linkFarm "flakelet-${name}-units" evaluated.units;
  }
  // pkgs.lib.optionalAttrs (evaluated.generators != { }) {
    generators = pkgs.linkFarm "flakelet-${name}-generators" evaluated.generators;
  }
  // pkgs.lib.optionalAttrs (evaluated ? exports) {
    "exports.json" = json "exports.json" (resolveExports evaluated.exports);
  }
)
