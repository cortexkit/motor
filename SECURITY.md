# Security

## Reporting a vulnerability

Report vulnerabilities privately through GitHub's private vulnerability
reporting: open <https://github.com/cortexkit/motor/security/advisories/new>,
or use **Report a vulnerability** on the repository's **Security** tab. Do not
open a public issue or pull request for a vulnerability.

Include what an attacker controls, what they gain, and how to reproduce it. A
report about job isolation is most useful with the job command, the runner
version (`motor-runner --version`), and whether the host passes the operator
isolation tests ([docs/server-gate.md](docs/server-gate.md)).

## Threat model

Motor runs commands that coding agents choose, on a build server shared by many
jobs. It treats the **command and the checkout it runs against as untrusted**:
they may be wrong, careless, or written to escape. It trusts the build server's
operator, the runner's service account, the CortexKit daemon that verifies
callers, and the credential vault that holds the SSH signing key.

The rules below are stated precisely, with their tests, in
[docs/job-isolation.md](docs/job-isolation.md),
[docs/windows-jobs.md](docs/windows-jobs.md) and
[docs/compile-cache.md](docs/compile-cache.md).

### What a Linux job is isolated from

A Linux job runs as the unprivileged `motor-job` account in a transient systemd
service with its own empty root directory, PID namespace, network namespace,
private `/tmp` and devices, no new privileges and no capabilities.

- **The runner and the host.** The job cannot read or write the runner's state,
  Git mirrors, other jobs' sandboxes, or the operator's home. Host toolchains
  and a short list of system files are visible read-only. It has no `sudo`.
- **Other workspaces.** Only its own workspace and target are writable; sibling
  checkouts it asked for are read-only. Other callers' workspaces are not
  mounted.
- **The network**, by default. The job has only its own loopback interface.
  When outbound access is enabled on both client and runner and the job asks for
  it, it can reach public IPv4 addresses only: not the host, its gateway,
  private or link-local ranges, or anything else on the server's network.
- **The shared caches.** The Cargo registry, Git sources and the compile cache
  are mounted read-only. A job, its build scripts and procedural macros cannot
  add or change cache entries.
- **Credentials.** Only build variables (`RUST_*`, `CARGO_*`, `NEXTEST_*` and a
  few others) cross from the caller, and names containing `TOKEN`, `SECRET`,
  `PASSWORD`, `KEY` or `CREDENTIAL` are dropped. The SSH signing key never
  leaves the vault; the job never sees it, and SSH agent forwarding is off.
- **The caller's machine.** Nothing a job writes is copied back. The caller
  receives output and a list of changed paths.
- **Runaway resources**, in part. CPUs are pinned to the job's claim, memory is
  capped, and cancel or timeout kills the whole process tree.

### What a Windows job is isolated from

A Windows job runs as a standard user inside a fresh QEMU/KVM virtual machine
booted from a copy-on-write overlay of a sealed image. The VM has no network
device. On the host, QEMU runs as the separate `motor-qemu` account in a
confined systemd unit that can open only `/dev/kvm`, its overlay, its cache
disk and its control socket. The overlay is deleted after the job, so nothing
written to the system disk reaches a later job. Dependency preparation runs on
the host as a third account, `motor-prep`, before the VM boots.

### What is not isolated

- **The Linux kernel.** The Linux sandbox is a namespace and credential
  boundary on a shared kernel, not a virtual machine. A kernel vulnerability
  reachable by an unprivileged process escapes it. The Firecracker microVM
  components in this repository are not used by any runner backend.
- **Jobs in the same workspace.** A workspace persists between jobs for the
  same checkout, and so does its build output and, on Windows, its cache disk.
  A job can leave files that the next job in that workspace reads.
- **Jobs sharing `motor-job`.** All Linux jobs run as the same account.
  Separate mount and PID namespaces keep them apart, but they are not separate
  users.
- **Outbound data**, when network access is granted. A networked job can send
  anything it can read, including its workspace, to the internet.
- **Disk and CPU contention.** There is no per-job disk quota; a job can fill
  the shared filesystem while it runs. Admission refuses new work below a
  free-space floor, but does not stop a running job. When the host is
  oversubscribed, CPU sets overlap.
- **Merged default-branch code, for the compile cache.** Before reporting a
  fill tip, `ck-motor` confirms that the local default branch's name and commit
  match the default branch advertised by the configured `origin`. Successful
  checks are cached for at most 24 hours; changing the local tip or origin URL
  requires another check. Moving local branches or remote-tracking refs alone
  no longer authorizes a fill. A mismatch or failed check skips the fill, not
  the job. Failures suppress repeat queries and warnings for the same repository,
  URL, branch and commit for 60 seconds in memory only; they never authorize a
  fill. Tests: `local_tip_ahead_of_remote_has_no_tip`,
  `forged_origin_head_cannot_authorize_a_remote_feature_branch`,
  `current_client_unverified_default_tip_runs_and_prepares_without_fills`.
  Code admitted to the remote default branch, including build scripts, still
  runs as `motor-cache` and can write cache entries other repositories read.
  Callers sharing an instance must trust each other's merged default-branch
  code. The configured origin URL is not pinned: an agent that rewrites it can
  point verification elsewhere. That more visible configuration change is an
  accepted limit. The local module, its stored verification results and Git
  credential configuration remain trusted; the runner does not independently
  attest the tip or protect against a modified client. See
  [compile-cache.md](docs/compile-cache.md#verifying-the-fill-tip).
- **The SSH key holder.** Anyone holding the runner account's SSH access
  controls the runner: the protocol has no per-caller authorization below SSH,
  and the same socket accepts `drain` and `resume`. Caller authorization happens
  in `ck-motor`, which relies on the CortexKit daemon to verify callers.
- **Direct-job secrets.** When direct execution is enabled, each direct job
  gets a one-time secret with 256 bits of OS randomness. Its holder may attach,
  check status or cancel only that job; the original route needs no secret.
  Only the SHA-256 hash is kept in `state.json`, for the lifetime of the job,
  and fixed-length digests are compared in constant time. Invalid credentials
  and unknown job IDs receive identical refusals without runner I/O. The
  plaintext is sent only on the live submitting direct route, never in replay,
  status, runner frames or module logs. AFT and Basal are unchanged. The CLI
  prints the secret once on stderr unless `run --secret-file PATH` creates a
  new `0600` file instead; keep both secrets and captured stderr private.
  `attach` reads stdin or `--secret-file`, never argv or environment variables.
  File permissions do not protect against a hostile same-uid process that can
  read the file or process memory; this is a bearer capability, not a new user
  isolation boundary. See [direct runs](docs/module.md#direct-runs). Tests:
  `direct_secret_reattaches_replays_and_never_leaks`,
  `secret_file_is_private_new_and_never_overwrites`,
  `attach_never_accepts_an_argv_secret_or_echoes_it`.
- **The operator and the runner account.** They are trusted. A compromise of
  either compromises every job on the server.
- **Side channels.** Jobs share CPU caches, memory bandwidth and the host's
  timing; no mitigation is attempted.

### What a job can reach, in short

| | Linux job | Windows job |
| --- | --- | --- |
| Its workspace | Read-write, at the caller's path | Read-write, under `C:\m` |
| Sibling checkouts it requested | Read-only | Copies inside the VM |
| Other workspaces, runner state, mirrors | No | No |
| Toolchains | Host's, read-only | Baseline's, read-only |
| Shared compile cache | Read-only | Its workspace's own cache disk, read-write |
| Network | Loopback; public IPv4 only if granted | Loopback inside the VM only |
| Caller's machine | No | No |
| Host kernel | Shared | Separated by KVM |

## Supported versions

Security fixes go to the `main` branch. There are no maintained release
branches.
