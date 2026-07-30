//! Exports are free-form metadata a service module returns next to its units
//! (metrics endpoints, claimed ports, reverse-proxy hints, state folders).
//! flakelet stores them per generation and publishes the active generation's
//! exports to <runtime_dir>/exports/<name>.json for external consumers.

use crate::error::{Error, Result};
use crate::state::write_json_atomic;
use serde_json::Value;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

fn export_path(runtime_dir: &Path, name: &str) -> PathBuf {
    runtime_dir.join("exports").join(format!("{name}.json"))
}

/// Publish the exports of the active generation (empty exports still publish,
/// so consumers see attach/detach symmetrically).
pub fn publish(runtime_dir: &Path, name: &str, exports: &Value) -> Result<()> {
    write_json_atomic(&export_path(runtime_dir, name), exports)
}

pub fn unpublish(runtime_dir: &Path, name: &str) -> Result<()> {
    match fs::remove_file(export_path(runtime_dir, name)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io(format!("unpublish exports of {name}"))(e)),
    }
}

/// A tcp/udp port range claimed via `exports.ports.<name>`.
#[derive(Debug, PartialEq)]
struct PortClaim {
    protocol: String,
    from: u64,
    to: u64,
}

fn port_claims(exports: &Value) -> Vec<PortClaim> {
    let Some(ports) = exports.get("ports").and_then(Value::as_object) else {
        return Vec::new();
    };
    ports
        .values()
        .filter_map(|entry| {
            let (from, to) = match entry.get("port") {
                Some(Value::Number(n)) => (n.as_u64()?, n.as_u64()?),
                Some(Value::Object(range)) => (
                    range.get("from")?.as_u64()?,
                    range.get("to").and_then(Value::as_u64)?,
                ),
                _ => return None,
            };
            let protocol = entry
                .get("protocol")
                .and_then(Value::as_str)
                .unwrap_or("tcp")
                .to_string();
            Some(PortClaim { protocol, from, to })
        })
        .collect()
}

/// Fail if `exports` claims a port that `owner`'s exports already claim.
pub fn check_port_conflicts(
    service: &str,
    exports: &Value,
    owner: &str,
    owner_exports: &Value,
) -> Result<()> {
    for theirs in port_claims(owner_exports) {
        for ours in port_claims(exports) {
            if ours.protocol == theirs.protocol && ours.from <= theirs.to && theirs.from <= ours.to
            {
                return Err(Error::PortConflict {
                    service: service.into(),
                    port: ours.from,
                    protocol: ours.protocol,
                    owner: owner.into(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn overlapping_ports_are_rejected() {
        let mine = json!({ "ports": { "dns": { "port": 53, "protocol": "udp" },
                                      "web": { "port": { "from": 8000, "to": 8010 } } } });
        let udp53 = json!({ "ports": { "dns": { "port": 53, "protocol": "udp" } } });
        let tcp53 = json!({ "ports": { "dns": { "port": 53 } } });
        let tcp8005 = json!({ "ports": { "api": { "port": 8005 } } });
        let none = json!({ "metrics": [ { "port": 9100 } ] });

        let check = |theirs: &Value| check_port_conflicts("mine", &mine, "a", theirs);
        assert!(check(&tcp53).is_ok());
        assert!(check(&none).is_ok());
        assert!(matches!(
            check(&udp53),
            Err(Error::PortConflict { port: 53, .. })
        ));
        assert!(matches!(
            check(&tcp8005),
            Err(Error::PortConflict { ref owner, .. }) if owner == "a"
        ));
    }
}
