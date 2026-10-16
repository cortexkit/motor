# motor-module (`ck-motor`)

`ck-motor` is motor's client: a reserved module of the CortexKit daemon that
admits `exec-remote/v1` callers, captures snapshots of their checkouts, and
sends jobs to `motor-runner` over SSH. It also provides `ck motor run` for
scripts, and `ck motor attach JOB_ID` to reconnect with a job's one-time secret.
`run` prints that secret on stderr, or saves it to a new private file with
`--secret-file PATH`. `attach` reads it from stdin or `--secret-file PATH`, never
from an argument or environment variable. See
[direct runs and reattachment](../../docs/module.md#direct-runs) for examples,
status mode, cancellation and exit codes.

- [docs/module.md](../../docs/module.md): placement, configuration, callers,
  credentials and storage.
- [docs/snapshot-contents.md](../../docs/snapshot-contents.md): what a snapshot
  contains.
- [docs/wire-protocol.md](../../docs/wire-protocol.md): the protocol to the
  runner and the rules for deciding whether a command ran.
