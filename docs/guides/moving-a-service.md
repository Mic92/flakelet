# Moving a service to another machine

`flakelet export` packs a service's state, `flakelet import` unpacks it
elsewhere and starts the service. The service author does nothing extra as
long as state lives in `StateDirectory=` (see
[Writing a service → State](writing-a-service.md#state)).

## The short version

```console
hosta$ flakelet export web | ssh hostb flakelet import -
```

On hosta, `export` stops all units of `web`. It runs `web-dump.service` if
the service ships one. Then it tars every `StateDirectory=` folder, starts
the units again and streams a zstd archive to stdout.

On hostb, `import` builds `web` pinned to the exported revision. It checks
that the state folders are empty and extracts the archive into them. Then
it runs `web-restore.service` and activates the service, health probe
included.

If hostb's NixOS configuration already declares `web`, that entry and its
settings are used. Otherwise a manual entry is registered from the archive.

## Check first

```console
hosta$ flakelet export web --dry-run | jq
```

This prints what would be exported, or why the service cannot be
exported.

## Settings on the target

If hostb declares `web` in its NixOS configuration, those settings are
used. Otherwise the settings from the archive are used as-is. They may
contain paths to secrets or certificates that only exist on hosta. Pass a
complete replacement in that case:

```console
hostb$ cat web.json
{ "port": 8080, "tlsCert": "/run/secrets/web-cert" }
hostb$ flakelet import web.flakelet.tar.zst --settings web.json
```

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
works if the provider on both hosts announces `state` support (check with
`--dry-run`).

## Static users

uids need not match between hosts. For `User=` services and
`exports.state.extraFolders` the named user and group must already exist
on the target host ([why](../design.md#users-and-state-ownership)).

## What can go wrong

- *state folder not empty*: import refuses to overwrite. `flakelet remove
  --purge web` on the target clears a previous attempt.
- *build fails on the target*: nothing was extracted yet; a freshly
  registered manual entry is removed again.
- *health probe fails after restore*: normal rollback rules apply, the
  service is put on hold and `flakelet status` shows why.
