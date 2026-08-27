# Files on the machine

All plain JSON, readable with `jq`. Every file carries `"version": 1`;
flakelet migrates older versions on write and refuses newer ones. Corrupt
`state.json`/`manifest.json`: read-only commands treat the entry as never
deployed, mutating commands require `--force`, which rebuilds from the
newest intact generation.

| path | written by | content |
| ---- | ---------- | ------- |
| `/etc/flakelet/config.json` | NixOS module | machine config and declarative entries, world-readable, no secrets |
| `/etc/flakelet/providers.d/*.json` | provider modules | `{ "contract": "postgres/v1", "provision"?: exe, "state"?: { "dump", "restore" } }` |
| `/var/lib/flakelet/<name>/service.json` | `deploy`, `import` | manual entry; same shape as one `services.<name>` in config.json |
| `/var/lib/flakelet/<name>/state.json` | every mutating command, atomically | what is deployed now |
| `/var/lib/flakelet/<name>/lock`, `/var/lib/flakelet/lock` | flock | holder description; per-entry exclusive, global shared (exclusive for `gc`), taken global-then-entry. `status`/`diff` take none |
| `/var/cache/flakelet/` | eval user | nix eval/fetch cache |
| `/nix/var/nix/gcroots/flakelet/<name>/gen-<N>/` | update/activate | `manifest.json` + `root-*` symlinks: unit files, export drvs, settings store paths, driver, flake source and inputs |
| `/run/systemd/system/<unit>` | activation, `boot` | symlinks into the current generation |
| `/run/flakelet/exports/<name>.json` | activation, `boot`, `remove` | exports of the running generation |

## config.json

```jsonc
{
  "eval_user": "flakelet",
  "cache_dir": "/var/cache/flakelet",
  "state_dir": "/var/lib/flakelet",
  "gcroot_dir": "/nix/var/nix/gcroots/flakelet",
  "nixpkgs": "/nix/store/…-source",
  "adios": "/nix/store/…-adios-source",
  "flakelet_lib": "/nix/store/…-lib",
  "extra_modules": [],
  "eval": { "workers": 1, "max_memory_mb": null },
  "credentials": { "netrc_file": null, "access_tokens_file": null,
                   "ssh_key_file": null, "ssh_known_hosts_file": null },
  "services": {
    "grafana": {
      "flake": "github:me/grafana-svc",
      "output": "flakelets.default",
      "settings": { "port": 3000, "tlsCert": "/run/secrets/grafana-tls" },
      "prebuilt": null,
      "input_overrides": {},
      "keep_generations": 5,
      "credentials": null            // per-entry override of the global block
    }
  }
}
```

## state.json

```jsonc
{
  "origin": "declarative" | "manual",
  "generation": 4,               // null if never deployed
  "units": { "grafana.service": "/nix/store/…/grafana.service" },
  "locked_url": "github:me/grafana-svc/<rev>",
  "pin": null,                   // set by `lock`
  "hold": { "reason": "…", "settings_hash": "sha256-…", "flake_rev": "<rev>" } | null,
  "degraded": false,             // running a cached generation after an offline eval failure
  "last_error": null
}
```

`hold` is set when a deploy was rolled back. `update` skips a held entry
until the current `settings_hash` or `flake_rev` differ from the recorded
ones, or `--force` is given. `degraded` is set by `--offline-fallback` when
evaluation failed on the network and the previous units were kept.

## manifest.json (per generation)

```jsonc
{
  "units": { "grafana.service": "/nix/store/…/grafana.service" },
  "flake_url": "github:me/grafana-svc/<rev>?narHash=…",
  "flake_rev": "<rev>",
  "settings_hash": "sha256-…",
  "driver": "/nix/store/…-flakelet-driver.nix",
  "exports": { … },              // derivations replaced by out paths
  "state": {
    "folders": [ { "path": "/var/lib/grafana", "user": "grafana", "group": null, "dynamic": true } ],
    "dump": null, "restore": null
  },
  "created": 1767000000
}
```

## Artifact

See [service module reference → Artifact layout](service-module.md#artifact-layout).

## Export archive

zstd-compressed tar:

```
meta.json          format version, source host, flake_url/rev, settings_hash,
                   state, exports, consistency: "stopped"
service.json       entry as on disk
state/<i>.tar      one per state.folders[i], no owner info
requires/<claim>/  provider dump output, opaque
```
