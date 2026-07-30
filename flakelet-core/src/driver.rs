use crate::config::Config;
use serde_json::Value;
use std::fmt::Write;

/// One service entry in the driver expression.
pub struct DriverEntry<'a> {
    pub name: &'a str,
    /// Fully locked flake URL (rev + narHash) for a pure builtins.getFlake.
    pub locked_url: &'a str,
    pub locked_rev: &'a str,
    /// Attribute path below the flake outputs, e.g. "flakelets.default".
    pub output: &'a str,
    pub settings: &'a Value,
    pub settings_hash: &'a str,
}

/// Render the driver expression that is added to the store and evaluated with
/// nix-eval-jobs. Each attribute builds a self-describing artifact:
/// meta.json, units/<unit files> and an optional health-check script.
pub fn render(config: &Config, system: &str, entries: &[DriverEntry]) -> String {
    let nixpkgs = config
        .nixpkgs
        .as_ref()
        .map_or("<nixpkgs>".into(), |p| p.display().to_string());
    let mut out = String::new();
    let _ = writeln!(out, "let");
    let _ = writeln!(
        out,
        "  pkgs = import {nixpkgs} {{ system = {}; }};",
        nix_string(system)
    );
    if let Some(adios) = &config.adios {
        let _ = writeln!(out, "  adios = import {};", adios.display());
    }
    // Derivations in exports become their out paths, so consumers of the
    // published exports.json execute store paths of the running generation.
    out.push_str(
        r#"  resolveExports = v:
    if builtins.isAttrs v then
      (if v ? type && v.type == "derivation" then "${v}" else builtins.mapAttrs (_: resolveExports) v)
    else if builtins.isList v then map resolveExports v
    else v;
"#,
    );
    let _ = writeln!(out, "in {{");
    for e in entries {
        let extra_modules = config
            .extra_modules
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let adios_arg = if config.adios.is_some() {
            "inherit pkgs adios;"
        } else {
            "inherit pkgs;"
        };
        let meta = serde_json::json!({
            "version": 1,
            "name": e.name,
            "flake_url": e.locked_url,
            "flake_rev": e.locked_rev,
            "settings_hash": e.settings_hash,
        });
        let _ = writeln!(
            out,
            r#"  {attr} =
    let
      module = (builtins.getFlake {url}).{output} {{
        {adios_arg}
        name = {name};
        settings = {settings};
        extraModules = [ {extra_modules} ];
      }};
    in
    pkgs.linkFarm "flakelet-{raw_name}" ({{
      "meta.json" = pkgs.writeText "flakelet-{raw_name}-meta.json" {meta};
      units = pkgs.linkFarm "flakelet-{raw_name}-units" module.units;
    }}
    // (if module ? healthCheck then {{ health-check = module.healthCheck; }} else {{ }})
    // (if module ? exports then {{
      "exports.json" = pkgs.writeText "flakelet-{raw_name}-exports.json"
        (builtins.toJSON (resolveExports module.exports));
    }} else {{ }}));"#,
            attr = nix_string(e.name),
            url = nix_string(e.locked_url),
            output = e.output,
            name = nix_string(e.name),
            settings = json_to_nix(e.settings),
            meta = nix_string(&meta.to_string()),
            raw_name = e.name,
        );
    }
    out.push_str("}\n");
    out
}

/// Convert a JSON settings value to a Nix expression.
pub fn json_to_nix(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => nix_string(s),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(json_to_nix).collect();
            format!("[ {} ]", inner.join(" "))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{} = {};", nix_string(k), json_to_nix(v)))
                .collect();
            format!("{{ {} }}", inner.join(" "))
        }
    }
}

fn nix_string(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace("${", "\\${")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_to_nix_conversion() {
        let v = json!({ "port": 3000, "tls": null, "flags": ["-v", true], "name": "a\"${x}\"" });
        assert_eq!(
            json_to_nix(&v),
            r#"{ "flags" = [ "-v" true ]; "name" = "a\"\${x}\""; "port" = 3000; "tls" = null; }"#
        );
    }

    #[test]
    fn render_driver() {
        let config = Config {
            nixpkgs: Some("/nix/store/aaa-source".into()),
            adios: Some("/nix/store/bbb-adios".into()),
            ..Config::default()
        };
        let settings = json!({ "port": 3000 });
        let expr = render(
            &config,
            "x86_64-linux",
            &[DriverEntry {
                name: "grafana",
                locked_url: "github:me/grafana-svc/abc?narHash=sha256-xyz",
                locked_rev: "abc",
                output: "flakelets.default",
                settings: &settings,
                settings_hash: "deadbeef",
            }],
        );
        assert!(
            expr.contains(r#"pkgs = import /nix/store/aaa-source { system = "x86_64-linux"; };"#)
        );
        assert!(
            expr.contains(r#"builtins.getFlake "github:me/grafana-svc/abc?narHash=sha256-xyz""#)
        );
        assert!(expr.contains(r#"settings = { "port" = 3000; };"#));
        assert!(expr.contains("inherit pkgs adios;"));
        assert!(expr.contains(r#"\"settings_hash\":\"deadbeef\""#));
    }
}
