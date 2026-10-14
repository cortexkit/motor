# motor-runner

`motor-runner` is motor's build-server daemon. It receives workspace snapshots
from `ck-motor` over SSH, keeps Git mirrors, admits and schedules jobs, runs
them in the Linux sandbox or a per-job Windows VM, and retains their output for
replay. `Config::new` without job isolation uses an in-process backend for
tests; it is not an isolation boundary.

- [docs/architecture.md](../../docs/architecture.md): how the runner fits in.
- [docs/job-isolation.md](../../docs/job-isolation.md): what a Linux job sees.
- [docs/windows-jobs.md](../../docs/windows-jobs.md): the Windows VM backend.
- [docs/scheduling.md](../../docs/scheduling.md): capacity, queueing, disk
  admission and retention.
- [docs/compile-cache.md](../../docs/compile-cache.md): the shared compile cache.
- [docs/runner-install.md](../../docs/runner-install.md) and
  [docs/drain-and-rollback.md](../../docs/drain-and-rollback.md): installing,
  deploying and draining.
- [docs/server-gate.md](../../docs/server-gate.md): the operator-only
  `isolated_` tests.
