use serde_json::Value;
use std::collections::BTreeSet;

/// Collect all /nix/store paths mentioned in string values of a settings JSON.
pub fn store_paths(value: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    collect(value, &mut out);
    out
}

fn collect(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::String(s) => {
            for p in extract_store_paths(s) {
                out.insert(p);
            }
        }
        Value::Array(a) => a.iter().for_each(|v| collect(v, out)),
        Value::Object(o) => o.values().for_each(|v| collect(v, out)),
        _ => {}
    }
}

/// Extract store paths embedded anywhere in a string (e.g. "--config /nix/store/x-foo/etc").
fn extract_store_paths(s: &str) -> Vec<String> {
    const PREFIX: &str = "/nix/store/";
    let mut res = Vec::new();
    let mut rest = s;
    while let Some(idx) = rest.find(PREFIX) {
        let tail = &rest[idx + PREFIX.len()..];
        // store path component: hash-name, ends at '/' or any char not allowed in store names
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || "+-._?=".contains(c)))
            .unwrap_or(tail.len());
        if end > 0 {
            res.push(format!("{PREFIX}{}", &tail[..end]));
        }
        rest = &tail[end..];
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_store_paths_in_nested_settings() {
        let settings = json!({
            "port": 8080,
            "tlsCert": "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-cert.pem",
            "cmdline": "--config /nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-cfg/etc/app.toml -v",
            "nested": { "list": ["/nix/store/cccccccccccccccccccccccccccccccc-data"] },
            "not_a_path": "/etc/passwd"
        });
        let paths = store_paths(&settings);
        assert_eq!(
            paths.into_iter().collect::<Vec<_>>(),
            vec![
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-cert.pem",
                "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-cfg",
                "/nix/store/cccccccccccccccccccccccccccccccc-data",
            ]
        );
    }
}
