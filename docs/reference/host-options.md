# Host options reference (`services.flakelets`)

For examples see [Host setup](../guides/host-setup.md).

| option                        | type                | default              | meaning |
| ----------------------------- | ------------------- | -------------------- | ------- |
| `enable`                      | bool                | `false`              | install the `flakelet` user, state/cache dirs, `flakelet-boot.service`, `flakelet-reconcile.service`, `flakelet.target` and render `/etc/flakelet/config.json` |
| `package`                     | package             | this flake's build   | flakelet binary |
| `nixpkgs`                     | path                | `pkgs.path`          | nixpkgs source the driver imports for all services |
| `adios`                       | path                | this flake's input   | adios source injected as `types` |
| `flakeletLib`                 | path                | `./lib`              | `flakelet.lib` used by the driver |
| `extraModules`                | listOf path         | `[ ]`                | files imported and passed to `impl` as `extraModules` |
| `eval.workers`                | positive int        | `1`                  | nix-eval-jobs workers |
| `eval.maxMemoryMb`            | null or int         | `null` (from RAM)    | restart an eval worker above this RSS |
| `credentials.netrcFile`       | null or str         | `null`               | passed as nix `netrc-file` |
| `credentials.accessTokensFile`| null or str         | `null`               | `host=token` lines, passed via `NIX_CONFIG` |
| `credentials.sshKeyFile`      | null or str         | `null`               | used in `GIT_SSH_COMMAND` with `IdentitiesOnly` |
| `credentials.sshKnownHostsFile`| null or str        | `null`               | strict host key checking for git+ssh |
| `services`                    | attrsOf *service*   | `{ }`                | declarative entries |

Credential files must be readable by the `flakelet` user and are checked
for existence before each update.

## *service* (`services.flakelets.services.<name>`)

| option                       | type          | default               | meaning |
| ---------------------------- | ------------- | --------------------- | ------- |
| `flake`                      | null or str   | `null`                | flake reference, resolved on the machine at update time |
| `output`                     | str           | `"flakelets.default"` | attribute path inside the flake |
| `settings`                   | JSON value    | `{ }`                 | checked against the module's `options`; store paths inside are verified and gc-rooted |
| `prebuilt`                   | null or path  | `null`                | artifact store path; mutually exclusive with `flake`/`settings`, nothing is evaluated |
| `inputOverrides`             | attrsOf str   | `{ }`                 | only `nixpkgs` is accepted; replaces the injected `pkgs` for this entry |
| `keepGenerations`            | positive int  | `5`                   | rollback depth |
| `autoUpdate.enable`          | bool          | `false`               | add `flakelet-<name>.timer` |
| `autoUpdate.interval`        | str           | `"daily"`             | `OnCalendar=` |
| `autoUpdate.randomizedDelay` | str           | `"1h"`                | `RandomizedDelaySec=` with `FixedRandomDelay=true` |

## Generated units

| unit                          | does |
| ----------------------------- | ---- |
| `flakelet-boot.service`       | relinks current generations into `/run/systemd/system` before `flakelet.target`; no network, no eval |
| `flakelet-reconcile.service`  | removes declarative entries that vanished from config.json; restarts when config.json changes; ordered before the per-service units |
| `flakelet-<name>.service`     | oneshot `flakelet update --offline-fallback <name>`; ordered after `network.target`, restarted with backoff on exit 75; restarts when the entry changes; after `flakelet-reconcile` and `flakelet-providers.target`, before `flakelet.target`; `Nice=10`, `IOSchedulingClass=idle`, `MemoryHigh=75%` |
| `flakelet-<name>.timer`       | with `autoUpdate.enable` |
| `flakelet.target`             | reached after boot relinking; order host units after it |
| `flakelet-providers.target`   | pulled in before every `flakelet-<name>.service`; providers add `wants`/`after` on their backing service so `provision` hooks work at boot |
