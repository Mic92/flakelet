# Moving a service to another machine

`flakelet export` packs a service's state, `flakelet import` unpacks it
elsewhere and starts the service. The service author does nothing extra as
long as state lives in `StateDirectory=` (see
[Writing a service → State](writing-a-service.md#state)).

## The short version

```console
hosta$ flakelet export web | ssh hostb flakelet import -
```

`export` stops all units of `web`, runs `web-dump.service` if the service
ships one, tars every `StateDirectory=` folder, starts the units again and
streams a zstd archive. `import` builds `web` on hostb pinned to the
exported revision, checks that the state folders there are empty, extracts
them, runs `web-restore.service` and activates, health probe included.

If hostb's NixOS configuration already declares `web`, that entry and its
settings are used. Otherwise a manual entry is registered from the archive.

## Check first

```console
hosta$ flakelet export web --dry-run | jq
```

prints what would be exported, or why it cannot be: never deployed,
currently degraded, built by a flakelet too old to record state, or a
`requires.*` claim whose provider cannot dump. `flakelet status --json`
shows the same under `export_blockers`.

## Settings with host paths

Settings travel in the archive, but paths to secrets or certificates are
host-specific. `--dry-run` lists them under `path_settings`. Supply
replacements on import:

```console
hostb$ cat web.json
{ "tlsCert": "/run/secrets/web-cert", "tokenFile": "/run/secrets/web-token" }
hostb$ flakelet import web.flakelet.tar.zst --settings web.json
```

`--settings` is ignored when hostb declares `web` itself.

## To a file

```console
hosta$ flakelet export web -o web.flakelet.tar.zst
hostb$ flakelet import web.flakelet.tar.zst
```

The archive holds no store paths and no secret contents. It does hold the
settings and the state, so treat it accordingly.

## Cloning under another name

Units and directories derive from the entry name, so importing under a new
name yields an independent copy, also on the same host:

```console
$ flakelet export web -o web.tar.zst
$ flakelet import web.tar.zst --name web-staging --settings staging.json
```

## Databases and other provider resources

A service with `exports.requires.postgres` does not dump the database
itself. `export` calls the postgres provider's dump hook, `import` calls
its restore hook, which also creates the database on the target. This only
works if the provider on both hosts announces `state` support; `--dry-run`
tells you.

## Static users

`DynamicUser=` state is extracted root-owned and systemd fixes ownership on
first start, so differing uids do not matter. For `User=` services and
`exports.state.extraFolders` the named user and group must already exist
on the target host.

## What can go wrong

- *state folder not empty*: import refuses to overwrite. `flakelet remove
  --purge web` on the target clears a previous attempt.
- *build fails on the target*: nothing was extracted yet; a freshly
  registered manual entry is removed again.
- *health probe fails after restore*: normal rollback rules apply, the
  service is put on hold and `flakelet status` shows why.
