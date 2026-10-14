# Building and qualifying the Windows job image

A Windows runner instance boots every job from one sealed baseline image. This
guide covers preparing the host for that instance, building the baseline,
qualifying it, updating its guest agent, and switching between baselines.
[windows-jobs.md](windows-jobs.md) states what jobs see once it runs.

The scripts are in `script/windows-vm/`; the host-side checks are in
`crates/motor-runner/src/windows.rs` and `windows/host.rs`. The scripts use
fixed paths under `/srv/build`; read each script's first lines before using it
on a differently laid out host.

The repository scripts the steps marked below. Creating the empty provisioning
disk, applying Windows updates before sealing, and copying the finished disk
into place are operator steps with no wrapper script; this guide states what the
runner requires of their result rather than prescribing one command.

## Host prerequisites

- A Linux build server with KVM, QEMU (`/usr/bin/qemu-system-x86_64`,
  `qemu-img`) and the runner already installed ([runner-install.md](runner-install.md)).
- Windows Server 2022 installation media and the virtio-win driver ISO under
  `/srv/build/windows-vm/media/`, as named in `launch-job-provisioning.sh`.
- Three distinct accounts besides the runner's: `motor-job` (shared with Linux
  jobs), `motor-qemu` (runs the VMs, member of `kvm`) and `motor-prep` (trusted
  dependency preparation). The runner refuses to start if any is missing or two
  are the same account.
- The instance layout at `/srv/build/motor-windows/<instance>`, owned by the
  runner's account, with `--state-root <layout_root>/state`. The runner checks
  every directory's owner, mode, filesystem and ACL at startup and refuses to
  start on any mismatch: `vm/`, `vm/sockets`, `vm/overlays`, `vm/caches`,
  `staging/`, `baselines/` (readable by `motor-qemu`), and the shared slot lock
  `/srv/build/motor-windows/slot.lock` (writable only by the owner and
  `motor-qemu`). The exact modes and ACLs are in `validate` in
  `crates/motor-runner/src/windows/host.rs`. Tests:
  `windows_startup_fails_closed_on_accounts_sudo_and_host_layout_acls`,
  `windows_startup_rejects_symlinked_layout_roots_and_different_state_roots`.
- The `sudo` rules the Windows backend uses to start its VM units and run
  preparation as `motor-prep`. Startup checks them along with the accounts and
  layout (same tests as above).
- For qualification only: `qemu-nbd`, `partprobe`, the kernel `ntfs3` module,
  and `hivexregedit` from `libwin-hivex-perl` (see `script/runner/tools.txt`).

The slot lock is shared with the standing Windows build VM
(`script/windows-vm/windows-build-vm.service`). Only one of the standing VM,
the provisioning VM and job VMs can run at a time.

## 1. Render the setup CD

Stop the standing VM if it runs, then render the job-image seed. Run from a
checkout so the guest agent's source is found:

```sh
sudo systemctl stop windows-build-vm.service
python3 script/windows-vm/prepare-seed.py --job-baseline
```

This writes `/srv/build/windows-vm/provision/seed/` and `seed.iso` (volume
label `CKSETUP`): an unattended-setup answer file with a random one-time
password, the guest setup scripts, and the `crates/motor-winagent` source. The
job image gets no SSH server and no authorized keys. The script refuses to run
when a previous `seed/` or `seed.iso` exists. Delete both after provisioning:
they contain the one-time password.

## 2. Provision the disk

The launch scripts expect their helpers (`slot-holder.py`) next to them in
`/srv/build/windows-vm/provision/`; stage `script/windows-vm/` there. Create an
empty qcow2 disk at `/srv/build/windows-vm/job-provisioning.qcow2` with
`qemu-img create -f qcow2`, sized for the toolchain and updates. Then boot it
as a transient system unit running as the trusted operator account that owns
`/srv/build/windows-vm`, with access to `/dev/kvm`:

```sh
sudo systemd-run --unit=windows-job-provisioning \
  --property=User=<operator> --property=SupplementaryGroups=kvm \
  /srv/build/windows-vm/provision/launch-job-provisioning.sh
```

`launch-job-provisioning.sh` attaches the installation media and seed CD and a
NAT network card without port forwarding (provisioning downloads updates and
toolchains; job VMs later boot with no network card at all). It takes the slot
lock and exits with status 75 if another VM holds it, naming the holder.

On first logon the guest runs `bootstrap.ps1`, which disables the built-in
administrator, randomizes local passwords and runs, in order:

1. `setup-guest.ps1 -JobBaseline`: Git, Bun and MSVC Build Tools.
2. `setup-job-baseline.ps1`: the standard `motor-job` user and its batch-logon
   right, a machine-wide Rust toolchain for `x86_64-pc-windows-msvc` under
   `C:\rust` (read-only to the job user), the virtio-serial driver, and the
   guest agent built from the seed's source with
   `cargo build --release --locked --target x86_64-pc-windows-msvc` and
   installed as an automatic LocalSystem service at
   `C:\ProgramData\Motor\motor-winagent.exe`. The agent's SHA-256 is written to
   `C:\build\provision\winagent.sha256`.

Success writes `C:\build\provision\provisioned`; failure writes
`C:\build\provision\FAILED.txt`. With no SSH, follow progress with
`python3 script/windows-vm/control.py screenshot` and send keys with
`script/windows-vm/console.py`.

`script/windows-vm/finalize-job-baseline.ps1` installs pending Windows updates,
rebooting up to five times, then powers the guest off. It is not on the default
seed; run it in the guest when the image should carry current updates, and
check its `finalize.log` for remaining updates before sealing.

Shut the guest down cleanly (`python3 script/windows-vm/control.py shutdown`
waits for a guest-initiated shutdown) before sealing.

## 3. Seal the baseline

The runner accepts a baseline only as a regular file, not a symlink, at
`<layout_root>/baselines/<name>`, owned by the runner's account, with mode
exactly `0440` and a read ACL for `motor-qemu`. Copy the provisioned disk there
under a new name; never overwrite a baseline a VM may be using. Test:
`windows_open_requires_vm_config_and_a_sealed_regular_baseline`.

## 4. Configure the instance

Write `<layout_root>/vm.toml`:

```toml
layout_root = "/srv/build/motor-windows/windows"
baseline = "<name>"          # a file name inside layout_root/baselines
memory_mib = 16384           # guest memory
cpu_set = [12, 13, 14, 15]   # the only value this build accepts
toolchain_version = "1.99.0" # the Rust version in the baseline
# overlay_growth_quota_bytes = 42949672960   # optional; default 40 GiB
```

Unknown keys are refused. `cpu_set` must equal the fixed set hard-coded in
`VmConfig::load`; the QEMU unit and the provisioning script use the same CPUs.
`toolchain_version` is part of each cache disk's key, so changing it starts the
caches cold.

The instance runs as `motor-runner@windows.service` with the drop-in
`script/runner/units/motor-runner@windows.service.d/windows.conf`:

```sh
motor-runner-windows serve --instance windows \
  --state-root /srv/build/motor-windows/windows/state \
  --backend windows-vm --vm-config /srv/build/motor-windows/windows/vm.toml
```

Install units with `script/runner/install-units.sh` and the binary with
`script/runner/deploy.sh <binary> windows`; see [runner-install.md](runner-install.md).

## 5. Qualify

Qualification boots the real image through the real runner binary. Run it on
the build server against the configured, otherwise idle Windows instance, with
the standing VM stopped:

```sh
cargo build -p motor-runner --bin motor-runner --release --locked
MOTOR_WINDOWS_VM_CONFIG=/srv/build/motor-windows/windows/vm.toml \
MOTOR_RUNNER_BINARY="$PWD/target/release/motor-runner" \
  cargo nextest run -p motor-runner --lib --locked --run-ignored only \
  -E 'test(/windows_qualification_/)'
```

Both variables are required; without them the tests fail rather than skip. The
binary must come from the same commit. The lane runs, from
`crates/motor-runner/src/tests/windows_operator.rs`:

| Test | Checks |
| --- | --- |
| `windows_qualification_baseline_machine_wide_toolchain_and_msvc_build` | The baseline toolchain builds an MSVC binary. |
| `windows_qualification_no_network_but_loopback_works` | No network beyond loopback. |
| `windows_qualification_qemu_and_preparation_accounts_are_confined` | `motor-qemu` and `motor-prep` confinement. |
| `windows_qualification_one_vm_slot_lock_refuses_without_boot` | A held slot refuses without booting. |
| `windows_qualification_cache_isolation_and_hard_cancel_discards_disk` | Cache disks stay per workspace; hard cancel discards. |
| `windows_qualification_cancel_kills_detached_grandchild` | Cancel kills the whole Job Object. |
| `windows_qualification_no_persistence_offline_overlay_canaries` | Nothing persists between jobs. |
| `windows_qualification_runner_sigkill_reconciles_boot_and_started_jobs` | Runner crash recovery. |
| `windows_qualification_build_environment_and_status_round_trip` | Environment, path mapping and status. |
| `windows_qualification_full_width_and_ntstatus_crash_round_trip` | Full 32-bit exit codes and crash status. |
| `windows_qualification_representative_build_and_dependency_cache` | A representative build and dependency cache reuse. |

Pass means every test passed; a skipped test is not a pass. Point `vm.toml` at a
baseline for production use only after it has qualified.

## Update the guest agent

To ship a new `motor-winagent` without reprovisioning, build it for
`x86_64-pc-windows-msvc` from `crates/motor-winagent` (a standalone crate with
its own `Cargo.lock`, outside the workspace), then patch a copy of the sealed
baseline offline:

```sh
cp --reflink=auto <layout_root>/baselines/<name> <candidate.qcow2>
chmod u+w <candidate.qcow2>
script/windows-vm/agent-swap.sh <candidate.qcow2> <motor-winagent.exe>
```

`agent-swap.sh` re-runs itself as root through `sudo -n`, attaches the
candidate with `qemu-nbd`, overwrites the agent in place (keeping its NTFS
permissions), and verifies the SHA-256 after remounting read-only. It never
opens the sealed original. Seal the candidate under a new name (step 3) and
qualify it (step 5) before using it.

## Switch or roll back a baseline

A baseline change takes effect when the runner restarts:

1. Seal the new baseline under a new name and qualify it.
2. Set `baseline` in `vm.toml` to the new name.
3. Restart the instance through `script/runner/deploy.sh <current binary> windows`,
   which drains first ([drain-and-rollback.md](drain-and-rollback.md)).

To roll back, set `baseline` back to the previous name and restart the same
way. Keep previous baselines in `baselines/` until the new one has run cleanly.
Cache disks are keyed by baseline name, so each switch starts with cold caches.

## Verify a running instance

```sh
systemctl --user status motor-runner@windows.service
journalctl --user -u motor-runner@windows.service -n 50
```

`runner.health` and `exec.status` with `platform: "windows"` report the
instance's units and its baseline toolchain. Test:
`windows_health_and_status_use_the_instance_units_and_baseline_toolchain`.
