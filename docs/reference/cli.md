# CLI reference

`flakelet [--config <file>] <command> [options]`. Default config is
`/etc/flakelet/config.json`. Mutating commands need root; `check`, `build`,
`driver`, `status`, `diff` do not. `flakelet <command> --help` prints the
same information with examples.

## Deploying

| command | does |
| ------- | ---- |
| `update [<name>…] [--force] [--no-wait] [--offline-fallback] [--no-refresh] [--flake <ref>]` | resolve, evaluate, build, activate. No names = all entries plus `reconcile`. `--force` retries a held entry. `--no-wait` fails instead of waiting for a lock. `--offline-fallback` keeps current units and exits 0 on network errors (used by the generated units). `--no-refresh` uses cached flake metadata. `--flake` deploys one entry from another ref once; the next plain update reverts. |
| `deploy <name> --flake <ref> [--settings <file>] [--output <attr>] [update options]` | register a manual entry in `/var/lib/flakelet/<name>/service.json` and update it |
| `activate <name> <store path>` | register and start a prebuilt artifact, no evaluation |
| `rollback <name>` | switch to the previous generation; the next update rolls forward again |
| `lock <name>` / `unlock <name>` | pin to / release the currently resolved revision |
| `remove [--purge] <name>` | stop, unlink, delete generations and bookkeeping, delete the exports file. State folders are kept and listed; `--purge` empties them |
| `reconcile` | remove declarative entries no longer in config.json |
| `gc [--keep <n>]` | prune generations beyond `keepGenerations` (or `<n>`) for all entries |
| `boot` | relink current generations; run by `flakelet-boot.service` |

## Inspecting

| command | does |
| ------- | ---- |
| `status [<name>…] [--json]` | generation, revision, held/degraded, lock holder, `export_blockers`, missing providers |
| `diff <name> [--no-refresh]` | `nix store diff-closures` between running generation and a fresh evaluation |
| `driver [<name>…] [--machine <m> [--flake <ref>]]` | print the generated driver expression |

## Off-machine / CI

| command | does |
| ------- | ---- |
| `check [<name>…] [--build] [--gc-roots-dir <dir>] [--machine <m> [--flake <ref>]]` | evaluate without touching state; `--build` also builds; warns about `requires.*` claims without provider |
| `build <name>… [--out-link <dir>] [--machine <m> [--flake <ref>]]` | like `check --build` with result symlinks (default dir `.`) |

`--machine <m>` renders the config from
`nixosConfigurations.<m>.config.services.flakelets.configFile` of `--flake`
(default: current directory) instead of reading `--config`. Manual entries
are only visible on their machine.

## Moving state

| command | does |
| ------- | ---- |
| `export <name> [-o <file>\|-] [--dry-run]` | stop units, run `<name>-dump.service` and provider dump hooks, tar `StateDirectory=` folders, start units, write zstd tar to stdout or `<file>`. `--dry-run` prints the would-be `meta.json` or the blockers |
| `import <file>\|- [--name <n>] [--settings <file>] [update options]` | build pinned to the exported revision, verify target folders are empty, extract, run provider restore hooks and `<name>-restore.service`, activate. Uses an existing/declared entry if present (then `--settings` is ignored), else registers a manual one. `--name` imports as a clone |

See [Moving a service](../guides/moving-a-service.md).

## Exit status

Non-zero on any failure, including a deploy that was rolled back. With
`--offline-fallback` a network failure during evaluation is exit 0 and
marks the entry degraded.

## Locks

Per-entry `flock` at `/var/lib/flakelet/<name>/lock`, global at
`/var/lib/flakelet/lock` (shared for entry operations, exclusive for `gc`).
Order is global then entry. `status`/`diff` take none and report a held
lock as "updating".
