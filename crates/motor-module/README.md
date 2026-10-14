# motor-module (`ck-motor`)

`ck-motor` is motor's client: a reserved module of the CortexKit daemon that
admits `exec-remote/v1` callers, captures snapshots of their checkouts, and
sends jobs to `motor-runner` over SSH. It also provides `ck motor run` for
scripts.

- [docs/module.md](../../docs/module.md): placement, configuration, callers,
  credentials and storage.
- [docs/snapshot-contents.md](../../docs/snapshot-contents.md): what a snapshot
  contains.
- [docs/wire-protocol.md](../../docs/wire-protocol.md): the protocol to the
  runner and the rules for deciding whether a command ran.
