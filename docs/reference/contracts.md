# Contracts and export schemas

`exports` is free-form JSON. The shapes below are the ones flakelet core or
known providers act on. All are published in
`/run/flakelet/exports/<name>.json`; providers watch that directory with a
path unit and reconcile from its full contents.

## Interpreted by flakelet core

### `exports.ports.<name>`

```nix
{ port = 9100; }                     # or { from = 4000; to = 4010; }
  // { protocol ? "tcp"; internal ? false; }
```

Activation is refused when two managed services claim the same port. No
schema file; firewall tooling may consume it. A service that wants the
host to own the socket ships a `.socket` unit instead.

### `exports.state.extraFolders`

```nix
[ "/srv/media" ]
```

Absolute, non-store paths carried by `export` in addition to
`StateDirectory=`. Constraints in the
[service module reference](service-module.md#derived-state-description).

## Blessed descriptions (schema in this repo)

### `exports.metrics`

```nix
[ { port = 9100; path ? "/metrics"; scheme ? "http"; } ]
```

Prometheus-style scrape targets.

### `exports.http.<name>` — `http/v1`

Schema: [`contracts/http-v1.json`](../../contracts/http-v1.json).
Constructor: `contracts.http { … }` (checks at eval time, fills defaults).

| field         | type            | default  |
| ------------- | --------------- | -------- |
| `host`        | string          | required |
| `upstream`    | `"unix:/run/…"` or `"host:port"` | required |
| `paths`       | listOf string   | `[ "/" ]`|
| `websockets`  | bool            | `false`  |
| `maxBodySize` | string          | `"1m"`   |
| `readTimeout` | string          | `"60s"`  |
| `buffering`   | bool            | `true`   |
| `extra.<impl>`| string          | —        |

Prefer unix-socket upstreams in `RuntimeDirectory=`: no port collisions,
works with `DynamicUser=`. TLS, public names and access policy are the
provider's business, not the service's.

Implementation: [flakelet-nginx](https://github.com/Mic92/flakelet-nginx)
(stateless, nothing to export).

## Claims (`exports.requires.*`, schema in the provider's repo)

### `exports.requires.postgres` — `postgres/v1`

```nix
{ database = name; role = name; }
```

Local socket, peer authentication, no password. The service connects as
`User=<role>` to `/run/postgresql`. Implementation:
[flakelet-postgres](https://github.com/Mic92/flakelet-postgres)
(export/import: not yet).

## Provider rules

- One provider per contract per host, announced in
  `/etc/flakelet/providers.d/<anything>.json`:
  `{ "contract": "postgres/v1" }`. Unknown keys are ignored. `check` and
  `status` warn about claims without an announcer; enforcement is a failed
  start plus `Restart=`.
- Level-triggered: `PathChanged=/run/flakelet/exports`, then reconcile
  everything.
- Provisioning is idempotent and add-only. Nothing is dropped on
  `remove`; orphans are listed by the provider and deleted by humans.
  Stateless renderings (vhosts) do converge, which is why `remove` deletes
  the exports file.
- No feedback channel to the service. Outcomes are deterministic from the
  claim, so the service bakes them into its units at eval time.
- Optional `"state": { "dump": "<exe>", "restore": "<exe>" }` in the
  announcement. Called by `export`/`import` as `<exe> <claim.json> <dir>`
  per claim. `restore` must create the resource if absent and refuse a
  non-empty one. Without `state` the provider's consumers are not
  exportable.
- Providers extend host services only through append-safe merge points
  (nginx `include`, SQL) and must coexist with an existing
  `services.nginx`/`services.postgresql`.

New blessed fields need a working implementation and a real consumer;
recurring `extra.<impl>` keys are the signal to promote one.
