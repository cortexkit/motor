# Installing the runner

This guide sets up a dedicated Linux build server to run `motor-runner`, and a
Mac to reach it with `ck-motor`. It covers the host, the sandbox accounts, the
tools jobs use, the compile cache, the systemd units, optional job networking,
and the first start. Later upgrades go through
[drain-and-rollback.md](drain-and-rollback.md); the Windows instance has its own
guide, [windows-image.md](windows-image.md).

The scripts under `script/runner/` encode one host layout: shared state under
`/srv/build`, a single operator account that owns the runner's systemd user
units, and Mac paths under `/Users`. Read each script's first lines before
running it; paths such as the operator's home and the network interface name
are fixed in the scripts and the unit files, and must match your host.

## Host requirements

- A dedicated Ubuntu server. The runner and its isolation tests start and stop
  system services; do not share the machine with unrelated workloads.
- An XFS filesystem with reflink support mounted at `/srv/build`. The runner
  clones workspaces and targets with reflinks, which work only within one
  filesystem, so caller paths (`/Users`) must live on the same filesystem.
  `setup.sh` bind-mounts `/srv/build/Users` at `/Users` and probes a reflink
  copy across them.
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

From a Mac with an SSH alias for the server (the scripts use `alfonso-runner`
unless `RUNNER_HOST` names another):

```sh
script/runner/setup.sh
```

`setup.sh` refuses to run unless `/srv/build` is a mounted XFS filesystem. It:

- installs the build packages (C toolchain, clang, lld, Git, Node, npm and
  others), and Rust, cargo-nextest and Bun at the versions the Mac has;
- creates `/srv/build/{src,target,cargo-home,tools-target,mirrors,bundles}`;
- bind-mounts `/srv/build/Users` at `/Users` through `/etc/fstab` and proves a
  reflink copy between them; it refuses if `/Users` already holds data;
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
and reloads systemd. It never restarts a runner. The units are:

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
| `--workspace-root PATH` | `/Users` | Adds a root under which idle workspaces may be evicted. |
| `--owned-workspace-root PATH` | `/Users` | Adds a root under which caller workspaces may be created. |
| `--backend host\|windows-vm` | `host` | Job backend. A third value, `vm`, is parsed but refused at startup in this build. |
| `--vm-config FILE` | absent | Windows instance configuration. |
| `--windows-cpus LIST` | absent | Linux placement avoidance: Windows VM host CPUs in CPU-list syntax; their online SMT siblings are included. Requires `--windows-slot-lock`. |
| `--windows-slot-lock PATH` | absent | Absolute path to the shared Windows QEMU slot lock; requires `--windows-cpus`. Read-only observation, never acquires the lock. |
| `--job-network veth` | absent | Allow outbound networking for jobs that request it. |
| `--reserve-daemon-core false` | true | Explicitly let a dedicated single-job sandbox use all online CPUs. The fleet default keeps its existing daemon-core reservation policy. |
| `--boat-profile` | absent | Explicit single-tenant Boat compatibility for systemd 255: omit unsupported PrivatePIDs, retain other isolation and add CPU/PID ceilings. Do not enable it to bypass missing PID isolation on an ordinary build server; the default profile refuses unsupported systemd. See `script/boat-image/README.md`. |
| `--drain-timeout-s N` | 1800 | Deadline for a SIGUSR1-initiated drain. |

`motor-runner --version` prints the version. Other subcommands are `connect`
(the SSH bridge `ck-motor` runs), `drain` and `resume`
([drain-and-rollback.md](drain-and-rollback.md)).

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
python3 script/runner/install-job-network.py            # show what would change
python3 script/runner/install-job-network.py --apply    # install
python3 script/runner/install-job-network.py --check    # verify
```

It installs `/etc/nftables.d/motor-net.nft` from `script/runner/motor-net.nft`,
`/etc/sysctl.d/90-motor-net.conf`, and a `motor-net-rules.service` that loads
the table at boot. It changes no other table. The rules name the host's uplink
interface and the job address range literally; edit `motor-net.nft` for your
host before applying.

Then add `--job-network veth` to the instance's `serve` line. At startup the
runner checks the table and prerequisites; when they fail it keeps running but
refuses network requests with `network_unsupported`. Prove the isolation with
the operator tests named in [job-isolation.md](job-isolation.md#6-network).

`script/runner/prove-job-network.py` deliberately weakens the rules to show the
network tests can fail. Run it only on dedicated test infrastructure.

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

## 8. Connect the Mac

Install `ck-motor` as described in [module.md](module.md): build it, place it
as a reserved module, enroll its signing key in the vault, add the public key
to the operator account's `authorized_keys`, and define the `alfonso-runner`
SSH alias.

## Verify

```sh
systemctl --user is-active motor-runner.service
journalctl --user -u motor-runner.service -n 50   # "motor-runner serving" with its version
```

Then run a job from the Mac, for example `ck motor run -- uname -a` (with
`admit_direct_exec` enabled; save its one-time secret for
[`ck motor attach JOB_ID`](module.md#reattaching-after-a-disconnect)), and the [server gate](server-gate.md) from a
checkout on the server.

## Uninstall

1. Drain: `motor-runner drain --instance main --exit-after-drain false`.
2. `systemctl --user disable --now motor-runner.service` and remove the unit
   files from `~/.config/systemd/user/`.
3. Remove the binaries from `~/.cargo/bin/` and the state root.
4. Remove optional components as described in their sections.

`setup.sh`'s host changes (packages, firewall, SSH policy, `/Users` bind mount)
are not reversed by any script.
