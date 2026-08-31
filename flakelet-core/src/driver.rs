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
    /// Locked nixpkgs URL from input_overrides.nixpkgs, replacing the shared pkgs.
    pub nixpkgs_override: Option<&'a str>,
}

/// Render the driver expression that is added to the store and evaluated with
/// nix-eval-jobs. Each attribute builds a self-describing artifact:
/// see lib/artifact.nix.
pub fn render(config: &Config, entries: &[DriverEntry]) -> String {
    let system = config.system.as_str();
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
    let _ = writeln!(out, "in {{");
    for e in entries {
        let extra_modules = config
            .extra_modules
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let entry_pkgs = match e.nixpkgs_override {
            Some(url) => format!(
                "import (builtins.getFlake {}).outPath {{ system = {}; }}",
                nix_string(url),
                nix_string(system)
            ),
            None => "pkgs".into(),
        };
        // Config::load guarantees both are set for flake-based services.
        let (lib_path, adios) = match (&config.flakelet_lib, &config.adios) {
            (Some(lib), Some(adios)) => (lib, adios),
            _ => panic!("driver requires flakelet_lib and adios in the config"),
        };
        let _ = writeln!(
            out,
            r#"  {attr} = import {lib_path}/artifact.nix {{ pkgs = {entry_pkgs}; adios = {adios}; }} {{
    name = {attr};
    module = (builtins.getFlake {url}).{output};
    settings = {settings};
    extraModules = [ {extra_modules} ];
    flakeUrl = {url};
    flakeRev = {rev};
    settingsHash = {hash};
  }};"#,
            attr = nix_string(e.name),
            url = nix_string(e.locked_url),
            rev = nix_string(e.locked_rev),
            hash = nix_string(e.settings_hash),
            output = e.output,
            lib_path = lib_path.display(),
            adios = adios.display(),
            settings = json_to_nix(e.settings),
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
        // A leading minus is an operator in Nix, not part of the literal.
        Value::Number(n) if n.to_string().starts_with('-') => format!("({n})"),
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
        let v =
            json!({ "port": 3000, "tls": null, "flags": ["-v", true, -1], "name": "a\"${x}\"" });
        assert_eq!(
            json_to_nix(&v),
            r#"{ "flags" = [ "-v" true (-1) ]; "name" = "a\"\${x}\""; "port" = 3000; "tls" = null; }"#
        );
    }

    #[test]
    fn render_driver() {
        let config = Config {
            nixpkgs: Some("/nix/store/aaa-source".into()),
            adios: Some("/nix/store/bbb-adios".into()),
            flakelet_lib: Some("/nix/store/ccc-flakelet-lib".into()),
            system: "x86_64-linux".into(),
            ..Config::default()
        };
        let settings = json!({ "port": 3000 });
        let expr = render(
            &config,
            &[DriverEntry {
                name: "grafana",
                locked_url: "github:me/grafana-svc/abc?narHash=sha256-xyz",
                locked_rev: "abc",
                output: "flakelets.default",
                settings: &settings,
                settings_hash: "deadbeef",
                nixpkgs_override: None,
            }],
        );
        assert!(
            expr.contains(r#"pkgs = import /nix/store/aaa-source { system = "x86_64-linux"; };"#)
        );
        assert!(
            expr.contains(r#"builtins.getFlake "github:me/grafana-svc/abc?narHash=sha256-xyz""#)
        );
        assert!(expr.contains(r#"settings = { "port" = 3000; };"#));
        assert!(expr.contains(
            r#"import /nix/store/ccc-flakelet-lib/artifact.nix { pkgs = pkgs; adios = /nix/store/bbb-adios; }"#
        ));
        assert!(expr.contains(r#"module = (builtins.getFlake "github:me/grafana-svc/abc?narHash=sha256-xyz").flakelets.default;"#));
        assert!(expr.contains(r#"settingsHash = "deadbeef";"#));
    }

    #[test]
    fn render_driver_with_nixpkgs_override() {
        let config = Config {
            nixpkgs: Some("/nix/store/aaa-source".into()),
            adios: Some("/nix/store/bbb-adios".into()),
            flakelet_lib: Some("/nix/store/ccc-flakelet-lib".into()),
            system: "x86_64-linux".into(),
            ..Config::default()
        };
        let settings = json!({});
        let expr = render(
            &config,
            &[DriverEntry {
                name: "svc",
                locked_url: "github:me/svc/abc?narHash=sha256-xyz",
                locked_rev: "abc",
                output: "flakelets.default",
                settings: &settings,
                settings_hash: "h",
                nixpkgs_override: Some("github:NixOS/nixpkgs/def?narHash=sha256-npk"),
            }],
        );
        assert!(expr.contains(
            r#"pkgs = import (builtins.getFlake "github:NixOS/nixpkgs/def?narHash=sha256-npk").outPath { system = "x86_64-linux"; };"#
        ));
    }
}
