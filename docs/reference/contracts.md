# Contracts and export schemas

`exports` is free-form JSON, published per service as
`/run/flakelet/exports/<name>.json`. This page lists the shapes that
flakelet core or a known provider acts on, and what a provider has to
implement.

## Interpreted by flakelet core

### `exports.ports.<name>`

```nix
{ port = 9100; }                     # or { from = 4000; to = 4010; }
  // { protocol ? "tcp"; internal ? false; }
```

Activation is refused when two managed services claim the same port.
There is no schema file. Firewall tooling may read it. A service that
wants the host to own the socket ships a `.socket` unit instead.

### `exports.state.extraFolders`

```nix
[ "/srv/media" ]
```

Absolute, non-store paths that `export` carries in addition to
`StateDirectory=`. Constraints are in the
[service module reference](service-module.md#derived-state-description).

## Descriptions with a schema in this repo

### `exports.metrics`

```nix
[ { port = 9100; path ? "/metrics"; scheme ? "http"; } ]
```

Prometheus-style scrape targets.

### `exports.http.<name>` — `http/v1`

Build it with `contracts.http { … }`, which checks the fields at eval time
and fills the defaults. Schema:
[`contracts/http-v1.json`](../../contracts/http-v1.json).

| field         | type                             | default   |
| ------------- | -------------------------------- | --------- |
| `host`        | string                           | required  |
| `upstream`    | `"unix:/run/…"` or `"host:port"` | required  |
| `paths`       | listOf string                    | `[ "/" ]` |
| `websockets`  | bool                             | `false`   |
| `maxBodySize` | string                           | `"1m"`    |
| `readTimeout` | string                           | `"60s"`   |
| `buffering`   | bool                             | `true`    |
| `extra.<impl>`| string                           | —         |

Prefer a unix socket in `RuntimeDirectory=` as upstream. It cannot
collide with another port and works with `DynamicUser=`. TLS, public
names and access policy belong to the provider.

Provider: [flakelet-nginx](https://github.com/Mic92/flakelet-nginx).
Stateless, so there is nothing to export.

## Claims with a schema in the provider's repo

### `exports.requires.postgres` — `postgres/v1`

```nix
{ database = name; role = name; }
```

The service connects as `User=<role>` to `/run/postgresql` with peer
authentication and no password.

Provider: [flakelet-postgres](https://github.com/Mic92/flakelet-postgres).
Supports export/import.

## Writing a provider

A provider announces itself with one file in `/etc/flakelet/providers.d/`.
The file name does not matter. Unknown keys are ignored.

```json
{
  "contract": "postgres/v1",
  "provision": "/nix/store/…/bin/provision",
  "state": {
    "dump": "/nix/store/…/bin/dump",
    "restore": "/nix/store/…/bin/restore"
  }
}
```

Only `contract` is required. `check` and `status` warn about claims that
no file announces. flakelet runs the hooks as root, once per
`requires.<contract>` claim:

| hook            | called as                  | when                                     |
| --------------- | -------------------------- | ---------------------------------------- |
| `provision`     | `<exe> <claim.json>`       | `update`/`activate`, before units switch |
| `state.dump`    | `<exe> <claim.json> <dir>` | `export`, while the service is stopped   |
| `state.restore` | `<exe> <claim.json> <dir>` | `import`, before the first activation    |

Rules:

- One provider per contract per host.
- `provision` is idempotent and add-only. A non-zero exit fails the update
  before any unit is touched. On NixOS, order the backing service before
  `flakelet-providers.target` so updates at boot can reach it.
- Nothing is dropped on `remove`. A rollback must find its database. The
  provider lists orphans and a human deletes them.
- Stateless renderings such as vhosts are the exception. They watch
  `/run/flakelet/exports` and reconcile from the whole directory, which is
  why `remove` deletes the exports file.
- There is no feedback channel to the service. Everything the service
  needs follows from the claim, so it bakes those values into its units at
  eval time.
- `restore` creates the resource if absent and refuses a non-empty one.
  Without `state` hooks the provider's consumers are not exportable.
- Providers extend host services only through append-safe merge points
  (nginx `include`, SQL) and coexist with an existing
  `services.nginx`/`services.postgresql`.

A new field earns a place in a schema once an implementation and a real
consumer exist. Recurring `extra.<impl>` keys are the signal to promote
one.
