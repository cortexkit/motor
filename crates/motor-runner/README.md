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

## Tenant VM mode

On Linux, `serve --backend host --tenant-vm` adds host-CID-only vsock
listeners on ports 52 (job connections) and 53 (durable lifecycle events).
On port 52 the host, after authenticating the client's job token, must write
`verified_lease` with the job ID, credential purpose and resource limits
before relaying the client's `hello`. The runner restricts operations to that
purpose and job, and matches Run's CPU, network, platform and timeout against
those limits. Admission expires at the lease's
`exp`; admitted work survives a client disconnect. The ordinary mode-0600
Unix socket and SSH hello exchange are unchanged. `--backend vm` remains
unavailable.

Lifecycle events live in `tenant-events.json` under the runner's instance
state directory. Port 53 replays the oldest unacknowledged event and advances
only on an ack for that event's sequence. Started and terminal events are
persisted with job transitions, even while no client or event relay is open.
Tenant job units add `TasksMax=4096` and deny `AF_VSOCK` for both offline and
outbound jobs, so neither can reach the host's control sockets.

`motor-runner tenant-agent` runs as root and serves port 54, also restricted
to the host CID. It handles `attach_tenant`, `post_restore`, `cancel`, `drain`,
`resume`, `status` and `args_status`. The image must provide `CARGO_HOME`
(image tools), `RUSTUP_HOME`, `HOME` and `PATH` in the agent's environment.
Fresh attachment mounts the XFS tenant disk at `/tenant`, binds the fixed
`/Users` and `/home` roots and the read-only image Cargo tools, and probes
cross-bind-mount reflinks before starting the transient
`motor-tenant-runner.service` unit. Runner options are read from
`/tenant/runner-args`; its state root is `/tenant/state` and Cargo home is
`/tenant/cargo-home`. Restore only refreshes the clock and entropy; it does
not remount the disk or restart the runner.
Attachment refuses symlinks, non-root-owned directories and writable-by-others
directories in persisted mount paths, and pins bind sources with open directory
descriptors. It removes only a stale socket node and waits until the runner
socket's peer PID matches the unit's current `MainPID`. Administrative forwarding
checks the same PID binding before sending any frames.

Portable `tenant_*` tests cover the operation fence, lease matching,
admission and lifecycle replay. The ignored operator-only test
`hosted_job_cannot_open_vsock` checks that a job cannot create a vsock socket
even when the runner can. It requires a trusted tenant VM with real isolation and
AF_VSOCK support, and must not run on a shared development host.
Run it inside the provisioned tenant image as part of image qualification,
where AF_VSOCK support and the image's job-isolation setup are available.
The ordinary host isolation suite selects names containing `::isolated_`;
this guest-only qualification is deliberately separate. Its exact command is:

```sh
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
  cargo test -p motor-runner --lib \
  isolation::tests::hosted_job_cannot_open_vsock -- --ignored --exact
```
