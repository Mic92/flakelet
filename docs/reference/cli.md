# CLI reference

`flakelet [--config <file>] <command> [options]`. Default config is
`/etc/flakelet/config.json`. Mutating commands need root. `check`, `build`,
`driver`, `status`, `diff` do not. `flakelet <command> --help` prints the
same information with examples.

## Deploying

| command | does |
| ------- | ---- |
| `update [<name>…] [--force] [--no-wait] [--offline-fallback] [--no-refresh] [--flake <ref>]` | resolve, evaluate, build, activate. No names = all entries plus `reconcile`. `--force` retries a [held](files.md#statejson) entry. `--no-wait` fails instead of waiting for another flakelet process. `--offline-fallback`: see exit status. `--no-refresh` uses cached flake metadata. `--flake` deploys one entry from another ref once; the next plain update reverts. |
| `deploy <name> --flake <ref> [--settings <file>] [--output <attr>] [update options]` | register a manual entry in `/var/lib/flakelet/<name>/service.json` and update it |
| `activate <name> <store path>` | register and start a prebuilt artifact, no evaluation |
| `rollback <name>` | switch to the previous generation; the next update rolls forward again |
| `disable <name> [-m <reason>]` | stop and unlink the units and mark the entry [disabled](files.md#statejson). Updates, host activation and reboots leave it alone |
| `enable <name>` | clear the mark and start the current generation. No evaluation, works offline |
| `lock <name>` / `unlock <name>` | pin to / release the currently resolved revision |
| `remove [--purge] <name>` | stop, unlink, delete generations and bookkeeping, delete the exports file. State folders are kept and listed; `--purge` empties them |
| `reconcile` | remove declarative entries no longer in config.json |
| `gc [--keep <n>]` | prune generations beyond `keepGenerations` (or `<n>`) for all entries |
| `boot` | relink current generations; run by `flakelet-boot.service` |

## Inspecting

| command | does |
| ------- | ---- |
| `status [<name>…] [--json]` | generation, revision, held/degraded/disabled, lock holder, `export_blockers`, missing providers |
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
| `export <name> [-o <file>\|-] [--to <host>] [--copy] [--dry-run]` | stop units, run `<name>-dump.service` and provider dump hooks, tar `StateDirectory=` folders, write zstd tar to stdout or `<file>`, then leave the entry disabled (`--to` labels the reason). `--copy` starts the units again instead. `--dry-run` prints the would-be `meta.json` or the blockers |
| `import <file>\|- [--name <n>] [--settings <file>] [--replace] [update options]` | build pinned to the exported revision, verify target folders are empty (or clear them with `--replace`, providers see `FLAKELET_REPLACE=1`), disable the entry, extract, run provider restore hooks and `<name>-restore.service`, activate. A failure after extraction empties the folders and leaves the entry disabled. Uses an existing/declared entry if present (then `--settings` is ignored), else registers a manual one from the archived flake ref with `--settings` (default none). `--name` imports as a clone |

See [Moving a service](../guides/moving-a-service.md).

## Exit status

Non-zero on any failure, including a deploy that was rolled back. A
disabled entry is not a failure. Network
errors exit 75 (`EX_TEMPFAIL`) so a service manager can retry just those;
with `--offline-fallback` and an existing generation they exit 0 and mark
the entry degraded instead.
