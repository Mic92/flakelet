# Service module reference

For a walk-through see [Writing a service](../guides/writing-a-service.md).

## Flake output

```
flakelets.<attr> = { types, ... }: { options = { … }; impl = { … }: { … }; }
```

The host selects `<attr>` with `output` (default `flakelets.default`). The
value may also be an attrset of such functions. The flake must not rely on
its own `inputs`; adios' `inputs`/`defaultFunc` wiring is not supported.

### Outer function arguments

| arg     | value                                          |
| ------- | ---------------------------------------------- |
| `types` | korora types as vendored by adios (`types.string`, `types.number`, `types.bool`, `types.listOf`, `types.attrsOf`, `types.option` (nullable), `types.struct`, …) |

### `options`

Attrset of `{ type; default ? ; description ? ; }`. Host `settings` are
checked against it before `impl` is called: unknown keys, type mismatches
and missing options without `default` abort the update.

### `impl` arguments

| arg            | value                                                                 |
| -------------- | --------------------------------------------------------------------- |
| `options`      | checked settings with defaults applied                                |
| `pkgs`         | host nixpkgs (or `inputOverrides.nixpkgs`), one instance per batch    |
| `name`         | entry name chosen by the host                                         |
| `contracts`    | constructors for blessed export schemas, see [contracts](contracts.md) |
| `storePath`    | `string -> string`: add string context to a `/nix/store/…` path from settings so the build depends on it |
| `extraModules` | list from `services.flakelets.extraModules`, imported                 |

## `impl` return value

All keys optional; unknown keys are errors. At least one unit must result.

| key             | type                                  |
| --------------- | ------------------------------------- |
| `services`      | attrsOf *service*                     |
| `sockets`       | attrsOf *unit* + `socketConfig`       |
| `timers`        | attrsOf *unit* + `timerConfig`        |
| `targets`       | attrsOf *unit*                        |
| `paths`         | attrsOf *unit* + `pathConfig`         |
| `healthCheck`   | string or derivation (executable path) |
| `dumpScript`    | string or derivation                  |
| `restoreScript` | string or derivation                  |
| `exports`       | attrsOf JSON-serialisable; derivations allowed and replaced by out paths |

### Unit naming

Attribute `${name}` → `<name>.<type>`. Any other attribute `foo` →
`<name>-foo.<type>`. flakelet refuses to activate a unit name already owned
by another managed service or by the host.

### *unit* (common to all types)

| key                                                                                        | rendered as               |
| ------------------------------------------------------------------------------------------ | ------------------------- |
| `description`                                                                              | `[Unit] Description=`     |
| `documentation`                                                                            | `[Unit] Documentation=`   |
| `after` `before` `wants` `requires` `requisite` `bindsTo` `partOf` `conflicts` `onFailure` | corresponding `[Unit]` key, list of strings |
| `unitConfig`                                                                               | extra `[Unit]` keys       |
| `wantedBy` `requiredBy`                                                                    | `[Install]`               |

`*Config` values: string, number, bool, path, derivation, or a list of
those (rendered as repeated keys).

### *service*

*unit* plus:

| key             | rendered as                                   |
| --------------- | --------------------------------------------- |
| `serviceConfig` | `[Service]`                                   |
| `environment`   | attrsOf scalar → `Environment=` lines         |
| `path`          | list of packages/strings → prepended to `PATH` (coreutils etc. are appended like NixOS) |

### Sugar units

| key             | generates          | `serviceConfig` defaults                                              | requires                          |
| --------------- | ------------------ | --------------------------------------------------------------------- | --------------------------------- |
| `healthCheck`   | `services.health`  | `Type=oneshot`, main unit's `User`/`Group`/`DynamicUser`/`StateDirectory`, `TimeoutStartSec=1min` | —                                 |
| `dumpScript`    | `services.dump`    | `Type=oneshot`, main unit's identity as above                         | `StateDirectory=` on `services.${name}` |
| `restoreScript` | `services.restore` | same                                                                  | same                              |

"Main unit" is `services.${name}`. With `DynamicUser=true` the sugar sets
`User=<name>` so systemd assigns the same uid. Each sugar is mutually
exclusive with defining the corresponding `services.<key>` yourself.

## Activation semantics

- All units of the previous generation are stopped in one job, disabled
  and unlinked before the new ones are linked.
- `failed` states are reset, then units with an `[Install]` section are
  enabled and started.
- Units without one are left to socket/timer/dependency activation.
- After switching, `<name>-health.service` is started if present; a failed
  start job, or any unit of the entry in `failed` state, rolls back.
- `<name>-dump.service` / `<name>-restore.service` are never started by
  activation, only by `export` / `import`.

## Derived state description

Written to `state.json` in the artifact, from `serviceConfig` only:

| source                                         | becomes                                                |
| ---------------------------------------------- | ------------------------------------------------------ |
| `StateDirectory=` of every service unit        | folder `/var/lib/<first component>`, owner from that unit; main unit wins on duplicates |
| `User=` / `Group=` / `DynamicUser=`            | folder owner; `dynamic = true` means extract root-owned |
| `exports.state.extraFolders`                   | extra absolute non-store paths, owner = main unit, requires static `User=` |
| presence of `services.dump` / `services.restore` | `dump` / `restore` unit names                        |

`CacheDirectory=`, `RuntimeDirectory=`, `LogsDirectory=` are ignored.

## Artifact layout

```
/nix/store/…-flakelet-<name>/
  meta.json      schema version, name, flake_url, rev, settings hash
  units/         rendered unit files
  exports.json   optional
  state.json     folders, owners, dump/restore unit names
```

`flakelet activate <name> <path>` and `services.<name>.prebuilt` consume
this directly.
