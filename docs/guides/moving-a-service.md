# Moving a service to another machine

`flakelet export` packs a service's state, `flakelet import` unpacks it
elsewhere and starts the service. The service author does nothing extra as
long as state lives in `StateDirectory=` (see
[Writing a service → State](writing-a-service.md#state)).

## Check first

```console
hosta$ flakelet export web --dry-run | jq
```

prints what would be exported, or why the service cannot be exported.

## Move

```console
hosta$ flakelet export web --to hostb | ssh hostb flakelet import -
```

hosta stops `web`, streams its state and leaves the entry **disabled**:
updates, `nixos-rebuild switch` and reboots no longer start it there.
hostb builds `web` at the exported revision, restores the state and
starts it ([step by step](../reference/cli.md#moving-state)).

If hostb's NixOS configuration declares `web`, that entry and its
settings are used. Otherwise a manual entry is registered from the flake
reference in the archive. Settings do not travel, so pass
`--settings <file>` if the service needs any.

Finally move the `services.flakelets.services.web` block from hosta's
configuration to hostb's and deploy both. hostb adopts the running entry
and stays on the imported revision until `flakelet unlock web`. hosta
forgets the disabled entry and lists the folders that still hold the old
data.

## If it did not work

hosta is disabled whether or not hostb succeeded. To run `web` there
again:

```console
hosta$ flakelet enable web
```

On hostb, a failure before extraction changed nothing. A failure after it
(restore hook, health probe) empties the folders again and leaves `web`
disabled there; fix the cause and repeat the import.

If hostb already ran `web` before the import, its folders are not empty
and import refuses. `--replace` clears them and tells provider restore
hooks to overwrite:

```console
hostb$ flakelet import web.flakelet.tar.zst --replace
```

## Copies and backups

`--copy` starts the service again after archiving instead of disabling
it, `-o` writes to a file:

```console
hosta$ flakelet export web --copy -o web.flakelet.tar.zst
```

Importing under another name gives an independent instance, also on the
same host:

```console
hosta$ flakelet import web.flakelet.tar.zst --name web-staging --settings staging.json
```

The archive holds no settings, store paths or secret contents, but it
does hold the service's data.

## Databases and other provider resources

A service with `exports.requires.postgres` does not dump the database
itself. `export` calls the postgres provider's dump hook and `import` its
restore hook, which also creates the database on the target. A provider
without hooks contributes nothing to the archive; an archive that carries
provider data the target cannot restore is refused.

## Static users

uids need not match between hosts. For `User=` services and
`exports.state.extraFolders` the named user and group must exist on the
target ([why](../design.md#users-and-state-ownership)).
