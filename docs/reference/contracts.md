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

A provider handles one kind of `requires.*` claim on a host. It announces
that with a file in `/etc/flakelet/providers.d/` (any name):

```json
{ "contract": "postgres/v1" }
```

From then on `flakelet check` and `flakelet status` stop warning about
`requires.postgres` claims on this host. One provider per contract per
host. Unknown keys in the file are ignored.

How the provider fulfils claims is up to it. Stateless ones (a vhost per
`http.*` export) watch `/run/flakelet/exports/` and re-render from the
whole directory whenever it changes. Providers that create something
persistent add hooks to the announcement. flakelet runs a hook as root,
once per service with a matching claim, and passes the claim as a JSON
file. There is no channel back to the service: everything it needs must
follow from the claim it wrote.

### `provision`

```json
{ "contract": "postgres/v1", "provision": "/nix/store/…/bin/provision" }
```

Called as `<exe> <claim.json>` during `update`/`activate`, before any unit
is switched. A non-zero exit fails the update. It must be idempotent and
add-only: nothing is dropped on `remove` or rollback, the provider lists
orphans and a human deletes them. On NixOS, order the backing service
before `flakelet-providers.target` so updates at boot can reach it.
Extend host services only through append-safe merge points (nginx
`include`, SQL) so an existing `services.postgresql` keeps working.

### `state.dump` / `state.restore`

```json
{
  "contract": "postgres/v1",
  "provision": "…",
  "state": { "dump": "/nix/store/…/bin/dump", "restore": "/nix/store/…/bin/restore" }
}
```

Each is optional. They make the provider's resource part of
[`flakelet export`/`import`](../guides/moving-a-service.md). A provider
whose claim carries no data (a DNS record, an OIDC client) defines
neither and `provision` on the target recreates it. Both are called as
`<exe> <claim.json> <dir>`: `dump` writes the resource into `<dir>` while
the service is stopped, `restore` reads it back on the target before the
first activation. `import` refuses an archive with data for a claim that
no provider on the target can restore. `restore` creates the resource if absent and refuses a
non-empty one, unless `FLAKELET_REPLACE=1` is set (`import --replace`),
in which case it may drop and recreate it.

A new field earns a place in a schema once an implementation and a real
consumer exist. Recurring `extra.<impl>` keys are the signal to promote
one.
