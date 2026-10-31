# Installing the runner

This guide sets up a dedicated Linux build server to run `motor-runner`, and a
client machine to reach it with `ck-motor`. It covers the host, the sandbox accounts, the
tools jobs use, the compile cache, the systemd units, optional job networking,
and the first start. Later upgrades go through
[drain-and-rollback.md](drain-and-rollback.md); the Windows instance has its own
guide, [windows-image.md](windows-image.md).

The scripts under `script/runner/` encode one host layout: shared state under
`/srv/build`, a single operator account that owns the runner's systemd user
units, and caller paths under `/Users` (where macOS clients keep their
checkouts) or under `/home`, served by caller-root mapping
([section 9](#9-optional-caller-roots-for-home-checkouts)). Read each script's
first lines before running it; paths such as the operator's home and the network
interface name are fixed in the scripts and the unit files, and must match your
host.

## Host requirements

- A dedicated Ubuntu server. The runner and its isolation tests start and stop
  system services; do not share the machine with unrelated workloads.
- An XFS filesystem with reflink support mounted at `/srv/build`. The runner
  clones workspaces and targets with reflinks, which work only within one
  filesystem, so caller paths must live on the same filesystem. `setup.sh`
  prepares the macOS client location, `/Users`: it bind-mounts
  `/srv/build/Users` at `/Users` and probes a reflink copy across them.
  Linux callers' `/home` checkouts are not prepared this way; they are stored
  under per-instance caller roots on the same filesystem instead
  ([section 9](#9-optional-caller-roots-for-home-checkouts)).
- systemd with `PrivatePIDs=` support (systemd 257 or newer; the runner does
  not check the version). The CPU
  accounting parser is tested against output captured from systemd 259. Also
  POSIX ACL tools (`setfacl`), and Linux namespaces and mounts.
- An operator account with `sudo`. The runner calls `sudo -n` for the
  operations that need root (starting job services, ACL repair, network
  attachment). The repository ships the narrow native-permissions fragment
  `script/runner/permissions-helper.sudoers`, not a complete host policy. Grant
  only the remaining command forms the runner uses. `script/runner/network-sudo-probes.json` lists
  the network forms; the rest are the `sudo` calls in
  `crates/motor-runner/src/isolation.rs`, `systemd.rs` and `network_linux.rs`.

## Native permission helper prerequisite

The trusted operator uses exactly these argv forms (paths are data, not shell):

```text
/usr/bin/sudo -n /usr/local/libexec/motor-runner-permissions physical-chmod owner-rwX -- /absolute/tree ...
/usr/bin/sudo -n /usr/local/libexec/motor-runner-permissions physical-chmod private-rwX -- /absolute/tree ...
```

Each path is a physical directory, repaired recursively, or a regular file,
repaired alone. Symlink and special-inode roots, and symlinked ancestors, are
refused before any mode changes.

Before deploying a candidate that uses native mode repair, an **administrator**
installs a matching copy and the operator-only sudo fragment:

```sh
sudo bash script/runner/install-permissions-helper.sh "$PWD/target/release/motor-runner" ubuntu
target/release/motor-runner check-permissions-helper
```

Use the actual non-root service owner instead of `ubuntu` if different. For
clarity, the privileged install operations performed by that administrator
script are equivalent to the following (prefer the script: it also validates
physical ownership, uses unique staging names and creates timestamped backups):

```sh
candidate="$PWD/target/release/motor-runner"
operator=ubuntu
policy=$(mktemp)
python3 script/runner/permissions_policy.py script/runner/permissions-helper.sudoers "$operator" "$policy"
sudo install -d -o root -g root -m 0755 /usr/local/libexec
sudo install -o root -g root -m 0755 "$candidate" /usr/local/libexec/.motor-runner-permissions.new
sudo install -o root -g root -m 0440 "$policy" /etc/sudoers.d/.motor-runner-permissions.new
sudo visudo -c -f /etc/sudoers.d/.motor-runner-permissions.new
sudo mv -f /usr/local/libexec/.motor-runner-permissions.new /usr/local/libexec/motor-runner-permissions
sudo mv -f /etc/sudoers.d/.motor-runner-permissions.new /etc/sudoers.d/motor-runner-permissions
sudo visudo -c
rm -f "$policy"
"$candidate" check-permissions-helper
```

The installer rejects `motor-job`, `motor-cache`, UID aliases of those accounts, root,
and unsafe account/template syntax. It validates physical root-owned ancestors,
installs an atomic root0755 binary at the fixed path and root0440 sudoers file,
and validates the fragment with visudo. The job account cannot write the binary
or its ancestors and receives no sudo rule. Only `physical-chmod`, the fixed
`owner-rwX` or `private-rwX` policy, and `--` are delegated; the native parser rejects relative/root
or symlink trees and never invokes a shell. No arbitrary mode/operation/binary
argument is accepted. This administrator bootstrap script itself is **not** a
delegated command.

`deploy.sh` checks the fixed helper's ownership, code digest and both
`sudo -n -l` forms before staging/draining a new daemon, so a missing grant cannot
silently break every job after the swap. Legacy candidates that do not use the
helper retain their previous deploy path. Administrator helper/policy updates
have timestamped backups but are not automatically reverted by daemon rollback;
restore matching helper/policy backups as administrator when reverting a
post-helper candidate. Ordinary daemon rollback/drain semantics are unchanged.
For example, after closing admission/draining all instances that use the helper,
an administrator can restore the installer's matching timestamped backups:

```sh
stamp=YYYYMMDDTHHMMSSZ
sudo install -o root -g root -m 0755 "/usr/local/libexec/motor-runner-permissions.rollback-$stamp" /usr/local/libexec/.motor-runner-permissions.restore
sudo install -o root -g root -m 0440 "/etc/sudoers.d/motor-runner-permissions.rollback-$stamp" /etc/sudoers.d/.motor-runner-permissions.restore
sudo visudo -c -f /etc/sudoers.d/.motor-runner-permissions.restore
sudo mv -f /usr/local/libexec/.motor-runner-permissions.restore /usr/local/libexec/motor-runner-permissions
sudo mv -f /etc/sudoers.d/.motor-runner-permissions.restore /etc/sudoers.d/motor-runner-permissions
sudo visudo -c
/path/to/matching-old-candidate check-permissions-helper
```

Restore/start the matching daemon using the drain-and-rollback procedure before
reopening admission; do not restore job state or rerun accepted commands. For a
first-install rollback with no backups, return every daemon to a pre-helper
version before removing the helper and `/etc/sudoers.d/motor-runner-permissions`.
Do not remove or downgrade a helper still needed by another running instance.

## 1. Bootstrap the server

From the operator machine, with an SSH alias for the server (the scripts use
`alfonso-runner` unless `RUNNER_HOST` names another):

```sh
script/runner/setup.sh
```

`setup.sh` refuses to run unless `/srv/build` is a mounted XFS filesystem. It:

- installs the build packages (C toolchain, clang, lld, Git, Node, npm and
  others), and Rust, cargo-nextest and Bun at the versions the operator machine
  has;
- creates `/srv/build/{src,target,cargo-home,tools-target,mirrors,bundles}`;
- bind-mounts `/srv/build/Users` at `/Users` through `/etc/fstab` and proves a
  reflink copy between them; it refuses if `/Users` already holds data;
- runs `script/runner/setup-caller-roots.sh`, which creates the per-instance
  caller-root directories beside that data unless they already hold something
  (see [section 9](#9-optional-caller-roots-for-home-checkouts));
- sets a firewall that denies inbound traffic except SSH, and an SSH policy of
  keys only, no root login, no agent or TCP forwarding; it fails if any other
  inbound allow exists or any other service listens on a non-loopback address
  (`script/runner/inbound_policy.py` is the check). The one named exception is
  a hosted host's agent port, accepted only with `RUNNER_HOST_AGENT_PORT` set;
  see [hosted-host.md](hosted-host.md#firewall);
- enables security-only unattended upgrades;
- installs pnpm 11.1.2 globally and writes `/srv/build/runner.env`;
- finishes by running `script/runner/sync.sh`, which proves a second SSH
  connection still works under the new policy.

## 2. Create the sandbox accounts

On the server:

```sh
sh crates/motor-runner/setup-accounts.sh
```

It creates `motor-job` and `motor-cache` as system accounts with no home and
`/usr/sbin/nologin`, if they do not exist, and prints their entries. It never
changes an existing account. At startup the runner checks both accounts and
refuses unsafe ones (for example one that is root, the runner's own account, or
administrative); an invalid `motor-cache` only disables compile-cache fills.
See [job-isolation.md](job-isolation.md#2-accounts).

## 3. Install the tools jobs use

Jobs run offline and see the shared Cargo home's `bin` read-only, so every
tool a repository needs must be installed beforehand:

```sh
bash script/runner/install-tools.sh
```

It installs each crate pinned in `script/runner/tools.txt` into
`/srv/build/cargo-home/bin`. A Git-sourced tool is installed once per revision
as `<bin>-<first 8 hex of rev>`, and the line marked `default` also owns the
plain name. `tools.txt` also lists the prerequisites it does not manage: Rust
toolchains and targets (install every pinned toolchain with `rustup`), Node,
pnpm and Bun under `/usr`, and system libraries.

Build the patched sccache. Stock sccache 0.18.0 updates an entry's file times
on every read, which fails on the read-only cache mount jobs get, so every
lookup would miss:

```sh
CARGO_HOME=/srv/build/cargo-home sh script/runner/build-sccache.sh
```

`build-sccache.sh` builds crates.io sccache 0.18.0 with
`script/runner/patches/sccache-0.18.0-read-only-hits.patch`, keeps the stock
binary it replaces as `sccache-0.18.0-stock`, and prints both hashes. See
[compile-cache.md](compile-cache.md).

## 4. Install the runner binary

Build on the server, or on another x86_64 Linux host with the same Rust version:

```sh
cargo build --release -p motor-runner --locked
target/release/motor-runner --version
```

The `main` instance's unit runs `~/.cargo/bin/motor-runner`; another instance
`<name>` runs `~/.cargo/bin/motor-runner-<name>`. For the first install, copy
the binary there with `install -m 755`. After that, always use
`script/runner/deploy.sh`, which drains the running daemon first.

## 5. Install the units

```sh
bash script/runner/install-units.sh
```

`install-units.sh` copies `script/runner/units/` into `~/.config/systemd/user/`, showing a diff
and asking before replacing a file that differs, keeps a timestamped backup,
and reloads systemd. It never restarts a runner, and it is the only script that
installs units. Before installing a runner unit file it runs, for each instance
whose effective `ExecStart` that file sets, that instance's binary with
`check-options` and the instance's serve arguments (section 9.2). A unit whose
arguments carry `--caller-root` is skipped, with the refusal named, when the
binary is absent, lacks `check-options`, or refuses them; the other files still
install. The units are:

| Unit | Runs |
| --- | --- |
| `motor-runner.service` | The `main` instance, with its capacity flags (`--max-weight`) and `--job-network veth` |
| `motor-runner@.service` | Any other instance, `serve --instance %i` |
| `motor-runner@e2e.service.d/binary.conf` | The `e2e` test instance, from its own binary |
| `motor-runner@windows.service.d/windows.conf` | The Windows instance; see [windows-image.md](windows-image.md) |

Edit capacity flags in the repository copy of the unit, not on the server: the
repository is the source of truth. Start the main instance the first time with:

```sh
systemctl --user enable --now motor-runner.service
```

The user manager must keep running without a login session for the units to
stay up (`loginctl enable-linger <operator>`).

### `serve` options

| Option | Default | Meaning |
| --- | --- | --- |
| `--instance NAME` | `main` | Instance name; state lives in `<state-root>/<instance>`. |
| `--state-root PATH` | `$XDG_DATA_HOME/cortexkit/motor-runner` | State root. |
| `--max-weight N` | 28 | vCPU budget for running jobs and setups. See [scheduling.md](scheduling.md). |
| `--max-queue-depth N` | 64 | Accepted jobs waiting to run. |
| `--rustc-slots N` | 24 | Machine-wide limit on concurrent compiler processes, enforced by the build-slot supervisor (`crates/prefrontal-build-slot`). |
| `--sccache-size-gib N` | 200 | Compile cache cap. |
| `--free-space-floor-bytes N` | 137438953472 (128 GiB) | Disk admission floor; 0 disables. |
| `--free-space-eviction-bytes N` | twice the floor | Optional soft reclaim goal, strictly above the hard floor. Does not change admission while the hard reserve is available. |
| `--build-folder-cap-bytes N` | 68719476736 (64 GiB) | After a job becomes idle, measure allocated workspace, target and incremental sizes read-only and cache them in memory for pressure ranking (missing entries are measured lazily). Above the cap, drop only `target/<profile>/incremental`, not the checkout or reusable outputs. A cleanup guard, not a quota. |
| `--workspace-root PATH` | `/Users` | Adds a root under which idle workspaces may be evicted. |
| `--owned-workspace-root PATH` | `/Users` | Adds a root under which caller workspaces may be created. |
| `--caller-root PREFIX=HOST` | absent | Maps caller paths beneath `PREFIX` onto `HOST`, a directory the runner owns, and presents them to the job at `PREFIX`. Repeatable. Needs isolated Linux host jobs; refused for Windows-VM and macOS-VM backends and for a tenant runner without `--boat-profile`. See [section 9](#9-optional-caller-roots-for-home-checkouts). |
| `--backend host\|windows-vm` | `host` | Job backend. A third value, `vm`, is parsed but refused at startup in this build. |
| `--vm-config FILE` | absent | Windows instance configuration. |
| `--windows-cpus LIST` | absent | Linux placement avoidance: Windows VM host CPUs in CPU-list syntax; their online SMT siblings are included. Requires `--windows-slot-lock`. |
| `--windows-slot-lock PATH` | absent | Absolute path to the shared Windows QEMU slot lock; requires `--windows-cpus`. Read-only observation, never acquires the lock. |
| `--job-network veth` | absent | Allow outbound networking for jobs that request it. |
| `--reserve-daemon-core false` | true | Explicitly let a dedicated single-job sandbox use all online CPUs. The fleet default keeps its existing daemon-core reservation policy. `--tenant-vm` implies `false`: a tenant VM is sized for its tenant's jobs, so it never keeps a core back (`tenant_mode_never_reserves_a_daemon_core`). |
| `--boat-profile` | absent | Explicit single-tenant Boat compatibility for systemd 255: omit unsupported PrivatePIDs, retain other isolation and add CPU/PID ceilings. Do not enable it to bypass missing PID isolation on an ordinary build server; the default profile refuses unsupported systemd. See `script/boat-image/README.md`. |
| `--drain-timeout-s N` | 1800 | Deadline for a SIGUSR1-initiated drain. |

`motor-runner --version` prints the version. Other subcommands are `connect`
(the SSH bridge `ck-motor` runs), `drain` and `resume`
([drain-and-rollback.md](drain-and-rollback.md)), and two that open no state:
`check-options <serve options>`, which runs the option parsing and the checks
that need no state directory and prints the first refusal, and
`forget-caller-root --prefix PREFIX <serve options>`
([section 9.4](#94-forget-caller-root)).

The main runner unit configures Windows CPU avoidance. Each new placement,
including background cache fills, stats the slot-lock file and reads `/proc/locks`
to check for its exclusive flock. This requires the host PID namespace's lock
table; acquiring or releasing a flock does not generate an inotify event. While
the slot is held, new jobs overlap only within the other allocatable CPUs. A
claim larger than that set gets the whole remaining set, without changing its
admission weight or resource quotas. The maximum claim and daemon-only core do
not change, and existing jobs keep their recorded CPU sets. After the slot is
released, new placements can use those cores again.
The test `capped_cpu_placement_keeps_requested_memory_limit` verifies that
`MemoryMax` follows the requested vCPU count even when `AllowedCPUs` is smaller.

A missing lock file is treated as no active Windows VM. Observation errors,
malformed lock entries, or a configuration that leaves no eligible CPUs fail
open to ordinary placement, with at most one warning per minute. Placement logs
include the observed Windows activity, excluded CPUs, selected CPUs, job ID, and
whether the job is a cache fill; health and job-record wire formats are unchanged.
Observation is a snapshot,
not a reservation against a Windows VM starting concurrently. The portable tests
`active_windows_excludes_siblings_and_overlaps_only_remaining_cpus` and
`windows_start_does_not_move_existing_job_or_exempt_cache_fills` cover placement;
`isolated_windows_slot_observation_tracks_flock_and_recreated_inode` checks the
real host lock table on an operator host.
The ignored test `windows_vm_running_linux_placement_avoids_its_cores` (run by hand, not by the operator lane) checks
that the Linux runner can see QEMU's lock, not just a lock created by the test
process. Leave a Windows job VM running on the dedicated server and run the test
as the Linux runner's user in its PID namespace; the test checks real systemd
activity before checking placement. Set `MOTOR_TEST_WINDOWS_SLOT_LOCK` and
`MOTOR_TEST_WINDOWS_CPUS` to the Linux runner's flag values before running it.

## 6. Optional: job networking

Outbound networking for jobs needs the `motor_net` nftables table, IPv4
forwarding, a narrow firewall route allowance, and the `sudo` forms in
`network-sudo-probes.json`. Install them with the operator-only installer,
which shows a diff by default:

```sh
python3 script/runner/install-job-network.py --uplink <uplink> --public-ip <public IPv4> # diff
sudo python3 script/runner/install-job-network.py --apply --uplink <uplink> --public-ip <public IPv4>
sudo python3 script/runner/install-job-network.py --check    # verify
```

It installs `/etc/nftables.d/motor-net.nft` from `script/runner/motor-net.nft`,
`/etc/sysctl.d/90-motor-net.conf`, a root-owned loader/policy module, and the
`motor-net-rules.service` boot unit. `--check` compares installed bytes/modes as
well as live rules, guarded tenant sets and owned UFW grants. Outside
`motor_net`, reconciliation changes only helper-owned tenant gate sets and
comment-owned tenant UFW routes; existing job rules and foreign policy remain.
The explicit uplink and its public IPv4 address are persisted in root-owned
`/etc/motor-network.json`; there is no default or runtime route sniffing. The
interface must exist, be UP and carry that address. When a hosted helper config
exists its settings must agree. The literal job address pool is unchanged;
rendering with `--uplink enp6s0` preserves the build server's original policy.
Interface names must be 1–15 ASCII bytes, start with an alphanumeric, contain
only letters, digits, `_`, `.` or `-`, and must not be `any` or `lo`.
Settings, the rendered file and the installed canonical template are checked
together under the network lock. A stale source for another uplink is refused
before any nftables or UFW call; installation holds the same lock while writing.

Every mutating policy load first removes `/run/motor-net-ready`. Only complete
base-policy and uplink success publishes a new root-owned generation there.
Tenant policy/UFW failures fence the affected tenant, not the host's own jobs;
`--check --host-only` checks that independent host readiness. The runner re-probes
the generation for each `net` admission and both before and after attachment,
so a failed startup probe never latches networking off. A missing generation
refuses that request with `NetworkUnsupported`; producer recovery lets the next
`net` request succeed without restarting the process. Non-net requests do not
probe networking. An old kernel table left by a failed reload cannot authorize
a new attachment. Host, Boat and tenant
guest policy producers all implement this protocol; old images without it fail
closed and must be rebuilt, not given a table-only compatibility fallback.

Attachment is authorized under the generation its final probe validated.
Publication/withdrawal is not synchronized with the later command-gate byte:
a later fence affects new jobs and tenant gates, and does not revoke an already
validated attachment. There is intentionally no extra gate-release probe.

### Deploy the producer before the consumer

The build-server runner is a **user** unit; `Before=motor-runner.service` in a
system producer unit does not order it. Correctness comes from per-request
re-probing, not cross-manager boot ordering. Use this rollout order:

1. Quiesce new net work while mixing an old table-only consumer with the new
   protocol. Check that `/etc/motor-network.json` agrees with the helper's
   explicit uplink/public IPv4 settings, and that the link is UP.
2. As the trusted runner account, run the network installer's read-only diff
   with `--uplink <uplink> --public-ip <public IPv4>` to check its sudo grants.
   Then apply it as root and run `--check --host-only`. This installs the
   producer first and requires a genuine root-published generation; never create
   a marker by hand to bypass a failure. The build server explicitly uses enp6s0.
3. Upgrade/restart the helper using the hosted-host procedure. Tenant faults
   may leave individual TAPs fenced without withdrawing host-job readiness.
   Inspect tenant health separately with the full network `--check`.
4. Run the ignored `isolated_net_` operator lane against the installed producer
   before replacing the runner. Do not count a missing-generation refusal as
   successful network qualification.
5. Deploy the new runner last, using the user-unit deployment procedure. Run a
   net canary and a non-net canary through the normal client. If a producer
   reload is unfinished or fails, that net request is refused; rerun the real
   producer after fixing it, then retry the canary in the **same runner**.
6. Resume net admissions only after the canaries pass. Producer-first minimizes
   mixed-version windows even though boot-time recovery no longer needs a
   runner restart.

For **Boat**, rebuild and roll the whole image containing the runner, producer
and marker helper together. Do not swap a new runner into an old image. For
**tenant VMs**, build a new tenant image with the matching runner, guest network
unit and marker helper, inventory/qualify it, then use the offline image-switch
procedure with the host/helper stopped. New consumers on old images fail closed.

The generation operator tests, like the other `isolated_net_*` cases, run as the
trusted **operator account**, not with `sudo cargo`, `sudo nextest` or a root test
binary. They use `sudo -n` only for private root-owned producer inputs,
publication/reload/cleanup and the existing privileged network helpers. This
keeps the shared operator-test lock owned and opened by the same account under
`fs.protected_regular`; do not chmod/chown/delete that host lock or change the
sysctl to run a test. The production marker is never withdrawn by these cases.
Ordinary network fixtures first run the producer's real read-only live-policy
checker (including canonical rules, uplink, forwarding, tenant gates and UFW),
then root-publish a generation to their scratch directory through the test-only
path override. No live rules are loaded or changed. This permits pre-deployment
testing when the live policy already matches but the old producer has no marker.
If `/etc/motor-network.json` is not installed, explicitly set both
`MOTOR_NET_TEST_UPLINK` and `MOTOR_NET_TEST_PUBLIC_IPV4`; no route sniffing or
interface default is used. The child process in
`isolated_net_restart_survives_and_cleans_up` reuses the parent's validated
private generation through the same scratch-path override. A policy mismatch
still fails closed: deploy producer/helper first,
then run the lane, then deploy the runner. Scratch generations do not relax the
production readiness protocol or track concurrent production reloads; keep
policy changes quiesced during the operator lane.
Run the default read-only diff as the trusted runner account to verify its
non-root `sudo` grants. Running `--check` as root skips verification of that
account's non-root `sudo` grants.

Then add `--job-network veth` to the instance's `serve` line. At startup the
runner checks the table and prerequisites; when they fail it keeps running but
refuses network requests with `network_unsupported`. Prove the isolation with
the operator tests named in [job-isolation.md](job-isolation.md#6-network).

To remove networking, drop `--job-network veth` from the unit and redeploy;
then stop `motor-net-rules.service` and delete the files the installer wrote.

## 7. Optional: review-copy reaper

```sh
bash script/runner/install-review-reaper.sh
```

It installs a root-owned timer that deletes checkouts under
`/srv/build/review` idle for six hours. Remove it with
`sudo systemctl disable --now motor-review-reaper.timer` and delete
`/usr/local/libexec/motor-review-reaper.py`,
`/etc/systemd/system/motor-review-reaper.service` and
`/etc/systemd/system/motor-review-reaper.timer`.

## 8. Connect the client machine

Install `ck-motor` as described in [module.md](module.md): build it, place it
as a reserved module, enroll its signing key in the vault, add the public key
to the operator account's `authorized_keys`, and define the `alfonso-runner`
SSH alias.

## 9. Optional: caller roots for `/home` checkouts

Linux callers keep their checkouts under `/home/<user>/...`. The runner never
stores one there: the build server's own `/home` holds the operator account,
its toolchains and the runner binary, and Boat's provider owns `/home` inside
its sandboxes. Instead each instance maps the `/home` prefix onto a directory it
owns, stores the checkout under that directory, and binds it back at the
caller's `/home` path, so compiler errors, stack traces and test output show the
caller's own paths. The job-side contract is in
[job-isolation.md](job-isolation.md#38-caller-root-mapping).

This section describes the rollout for a production host: it
changes what the build server runs, so it is done by the operator, not by a
script run from a pull request.

### 9.1 The two steps

Caller roots ship in two steps and never in one:

1. **Dormant binary.** Deploy a runner that understands `--caller-root` while no
   unit or Boat image configures one. Behaviour is then byte-identical to the
   previous release, so the step changes nothing a caller can observe.
2. **Flagged units.** Once the dormant release has run in production, the
   units gain `--caller-root`. From then on every rollback target is a binary
   that understands the flag.

The edits that add the flag to `script/runner/units/motor-runner.service`,
`script/runner/units/motor-runner@e2e.service.d/binary.conf` and
`script/boat-image/motor-boat.service` are one separate change. It merges only
at step two, after it has been reviewed on its own: it is the change that puts
the flag into what the build server and Boat actually run, so a reviewer checks
those unit files against the final flag configuration before anyone installs
them. It also merges only after `setup-caller-roots.sh` has run on the build
server. Until then the repository units stay unflagged, so a routine
`install-units.sh` run installs nothing flagged, and tests that need flagged
arguments use fixture copies of the units.

A downgrade to a binary that predates mapping, once caller roots were enabled on
an instance, is not supported. Two gates enforce that: `deploy.sh` before any
swap (9.3) and `install-units.sh` before installing a unit (5).

### 9.2 Preparation

On the build server, as the operator:

```sh
bash script/runner/setup-caller-roots.sh
```

It creates `/srv/build/callers/main/home` and `/srv/build/callers/e2e/home` with
the owner, mode and device of `/srv/build/Users`, proves a reflink copy with an
unnamed temporary file, and leaves both empty. It refuses a symlinked component,
a non-directory, a non-empty directory, and a directory on another device. It
does nothing else (no packages, no toolchain step), and the host's real `/home`
is not touched. `setup.sh` calls it, so a freshly provisioned host already has
the directories.

The serve arguments of a unit are its effective `ExecStart`, meaning the last
non-empty `ExecStart=` line with `%i` replaced by the instance, minus every word
up to and including the first `serve`. For the repository units the command is
```sh
python3 script/runner/deploy_canary.py --repository-unit-command script/runner/units <instance>
```
which prints them from `script/runner/units/` for the instance (`main`, `e2e`,
`windows`); `deploy.sh` and `install-units.sh` use the same extraction on the
installed unit.

### 9.3 Enablement

1. Run `setup-caller-roots.sh` (9.2).
2. Install the flagged units with `install-units.sh`. Each flagged unit is
   checked first with its instance's binary and serve arguments (5), and a
   refusal leaves that unit uninstalled.
3. Per instance, run `deploy.sh` with a copy of the installed binary as the
   candidate:

   ```sh
   script/runner/deploy.sh /path/to/installed/motor-runner main
   ```

   `deploy.sh` refuses before draining when the candidate's `check-options`
   refuses the installed unit's arguments, and when the candidate lacks
   `check-options` while the arguments carry `--caller-root` or the instance's
   `<state-dir>/caller-roots-ever-enabled` exists. Otherwise it drains, swaps,
   restarts with the flag and runs both canaries. The first start with the flag
   writes `caller-roots-ever-enabled`; nothing removes it, `forget-caller-root`
   included.

Executable rollback does not undo enablement: it restarts the same flagged unit
with the previous binary, which now starts with the flag. When the installed
arguments carry `--caller-root`, a failed deploy therefore names the flag-off
rollback (9.5) in its message.

### 9.4 `forget-caller-root`

`forget-caller-root` is the only way to retire a mapping. It requires the
instance to be stopped and refuses, changing nothing, while a live job unit
that belongs to one of that instance's jobs exists:

```sh
motor-runner forget-caller-root --prefix /home --instance main \
  --state-root <state-root> --caller-root /home=/srv/build/callers/main/home
```

Pass the instance's full serve argument list, exactly as the unit has it (the
list may still carry `--caller-root`; a different host directory than the
recorded one refuses and names both values). It renames every child of the
caller root's host directory into its trash, deletes the trees only its
`caller-catalog.json` entries own, purges the trash and removes those entries
and the mapped job records. A rerun converges after a kill at any point, and the
following start without the flag succeeds. The mechanism and its tests are in
[job-isolation.md](job-isolation.md#38-caller-root-mapping).

### 9.5 Flag-off rollback

1. Stop the instance.
2. Run `forget-caller-root` with the instance's extracted serve arguments, which
   still carry the flag.
3. Install the unflagged unit with `install-units.sh`.
4. Start it with `systemctl --user start <unit>`.
5. Run `deploy.sh` with a copy of the installed binary to drain and run the
   canary.

Restoring the unflagged unit alone is not enough: once any mapped record exists
(the canary's, for instance), a runner started without the flag refuses to start
until `forget-caller-root` has removed it. `deploy.sh` cannot start a stopped
instance, because its drain connects to the runner socket, which is why step 4
starts it directly.

### 9.6 Boat

Boat's runner maps `/home` to `/var/lib/motor-boat/state/callers/home` on the
state disk, outside the state directory and outside `state/workspaces`. Every
Boat image after the first flagged image carries the same mapping: an image
never drops it, and image verification resumes a state written by the previous
flagged image and runs a `/home` job against it. Downgrading a Boat image to one
without the mapping, once it was enabled, is the same unsupported downgrade as
on the build server.

## Verify

```sh
systemctl --user is-active motor-runner.service
journalctl --user -u motor-runner.service -n 50   # "motor-runner serving" with its version
```

Then run a job from the client machine, for example `ck motor run -- uname -a` (with
`admit_direct_exec` enabled; save its one-time secret for
[`ck motor attach JOB_ID`](module.md#reattaching-after-a-disconnect)), and the [server gate](server-gate.md) from a
checkout on the server.

## Uninstall

1. Drain: `motor-runner drain --instance main --exit-after-drain false`.
2. `systemctl --user disable --now motor-runner.service` and remove the unit
   files from `~/.config/systemd/user/`.
3. Remove the binaries from `~/.cargo/bin/` and the state root.
4. Remove optional components as described in their sections.

`setup.sh`'s host changes (packages, firewall, SSH policy, `/Users` bind mount,
caller-root directories) are not reversed by any script. Retiring a mapping is a
separate, explicit step ([section 9.4](#94-forget-caller-root)).

## Optional outbound monitor metrics

Monitor metrics are off by default. The Windows-VM instance managed by
`script/runner/install-units.sh` can send signed, outbound-only monitor samples
every 60 seconds. The signing key stays under the runner's private state folder,
`STATE_ROOT/windows/monitor/host-key`; it is separate from job and placement keys.
Supply both an HTTPS origin (no credentials, path, query or fragment) and a host
id matching `[A-Za-z0-9._-]{1,64}`:

Export `TMPDIR` pointing to the shared scratch directory before rendering units.

```sh
bash script/runner/install-units.sh \
  --metrics-endpoint https://metrics.example --metrics-host-id monitor-1
```

The installer shows the rendered flags in its diff and asks before installing.
Only `motor-runner@windows` gets the metrics flags; other instances are unchanged.
It reloads systemd's unit definitions but never restarts the runner. Accept the
Windows drop-in and restart through `script/runner/deploy.sh` at the next planned
deployment; that script drains the running daemon before swapping its binary. Omitting the pair renders the repository's metrics-disabled default.

Print (or create once) the stable public key as 64 hex characters, without
installing units or starting the service:

```sh
bash script/runner/install-units.sh --metrics-key
```

Run as the same operator account as the service. The key command uses the
Windows instance's binary, instance and state-root flags from the versioned
unit, so register that public key with the metrics receiver before the restart.
