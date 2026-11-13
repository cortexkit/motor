# macOS VM image and host installer

This recipe uses the repository's `motor-macvm image-build` command, which calls
`VZMacOSInstaller` for restore and adds graphics and NAT **only** in image-build
mode. Production jobs use neither networking nor graphics. No third-party VM
manager is used.

## Pin lock

`toolchain.json` is a **reviewed** lock (`release_status`). The restore pin
is measured: macOS 27.2, build **26B123**, 25,595,669,503 bytes, SHA-256
`6613b34e8589db2202a01ec37a2648afb141ece13428175183f8f3e38438534f`.
The file is Apple's `UniversalMac_27.2_26B123_Restore.ipsw` from Apple's macOS
restore image catalog; its SHA-1 matched the catalog's published digest on
download, and the SHA-256 above was measured from that verified file. `observed_host` describes that measurement's
host, not a required guest image version. Obtain the exact named IPSW from Apple
or the operator's retained copy; `restore` checks its size and hash before use.

Rust versions and components are derived from the read-only build-server
manifest. The rebuild recipe also installs Rust **1.100.0** beside **1.99.0**, with
the same components and native target; **1.99.0 remains the default** and the source
of the independent stable copy. Rust 1.92.0, `llvm-tools`, `rust-analyzer` and Bun
1.4.4 are included. CMake **4.2.3**, matching the Linux manifest, comes from
Kitware's official `cmake-4.2.3-macos-universal.tar.gz`, pinned by SHA-256
`c2302d3e9c48daabee5ea7c4db4b2b93b989bcc89dae8b760880e00120641b5b`
from the release's `cmake-4.2.3-SHA-256.txt`. Its CLI binaries (`cmake`, `ctest`,
`cpack`) and module tree are installed read-only under `/opt/motor/cmake`, with
links in `/opt/motor/bin` on the job PATH; the GUI and Qt frameworks are excluded.
GNU bash 5.2.37 is built from its pinned GNU archive (the source hash is
also published in Buildroot's `package/bash/bash.hash`). Git uses the server's
pinned kernel.org source release. The agent source is pinned to the full merged
Motor commit in the `agent.commit` value in `toolchain.json`; `git archive` exports that commit, not the current
working tree. The guest build records the installed agent's SHA-256.

The CLT receipt version and native binary hashes cannot be inferred from Linux
artifacts; a new lock starts with them pending, never fabricated. `--measure` is the
only path allowed to bootstrap pending inputs. It writes a **candidate**, does not
mark it reviewed, and cannot seal or install an image. Normal provisioning,
sealing, installation and parity checking fail until every pending field is
filled and `release_status` is `reviewed`. Sealing additionally requires the lock
to be tracked and unchanged against HEAD. Review and commit the measured lock
before deploying it. Re-run measurement and reseal after any agent/protocol or
pin change; do not change only an artifact hash to make a check green.

## Dedicated-host operator procedure

Use a dedicated VM-building session on the macOS host, not a job VM or an
operator's unrelated workstation. Host prerequisites: Apple Silicon, APFS
storage (internal by default, or an explicitly selected fixed-media data volume
with ownership enabled; see installation below), Command Line Tools (including
Swift and Python 3.9+), the native
`motor-macvm` binary built from reviewed Motor source, and enough free space for
the IPSW, sparse 96 GiB boot disk, Rust toolchains and exported tools. Keep scratch
inside the checkout, for example `target/scratch/macos-image`.

1. Generate the **build-only** SSH key:

   ```sh
   python3 script/macos-runner/image.py key --work target/scratch/macos-image
   ```

2. On the Mac's desktop through Screen Sharing, run restore with the signed native
   helper. This prepares the four bundle files using the restore image's supported
   hardware model, then opens the helper's display:

   ```sh
   python3 script/macos-runner/image.py restore \
     --work target/scratch/macos-image --image "$IMAGE" \
     --vm-helper "$MACVM" --ipsw "$IPSW"
   ```

   Do the one Setup Assistant pass. Create the temporary admin `motorbuild`,
   enable guest Remote Login with "Allow full disk access for remote users" on
   (sealing turns Remote Login off from SSH, which macOS refuses without it),
   authorize only `build_key.pub` for that account,
   and temporarily allow its passwordless sudo in `/etc/sudoers.d/motorbuild`.
   Obtain the guest sshd host key from its console and put it in a dedicated
   known-hosts file; do not use an unchecked `ssh-keyscan` result as trust.
   Shut down after restore if necessary and use `image.py boot` (same arguments
   except `--ipsw`) to resume the **same** builder image in another terminal.

3. In the guest console inspect `softwareupdate --list` for the exact CLT label.
   Seed a separate rebuild manifest; the committed `toolchain.json` continues to
   describe the sealed image until its replacement has been measured and reviewed.
   The seed includes all build-server toolchains/components even when parity has
   pending exceptions for the live image. Provision over SSH with strict host-key
   checking:

   ```sh
   python3 script/macos-runner/pinlock.py --seed > target/scratch/macos-image/rebuild.json
   python3 script/macos-runner/image.py provision --measure \
     --manifest target/scratch/macos-image/rebuild.json \
     --work target/scratch/macos-image --guest motorbuild@GUEST_IP \
     --known-hosts "$GUEST_KNOWN_HOSTS" --clt-label "$CLT_LABEL"
   ```

   The bootstrap installs that exact softwareupdate item. Apple authenticates the
   update; the guest checks the CLT receipt with `pkgutil --pkg-info-plist` and
   verifies the installed clang's Apple code signature. There is no invented CLT
   package-file hash: softwareupdate does not retain a package here.

   GitHub payloads (Bun, CMake and pinned Cargo source archives) try six parallel
   HTTP ranges, verify response ranges/lengths, then verify the complete SHA-256.
   If range service is unavailable, the downloader uses a serial transfer with
   the same digest check. Rustup verifies the Rust distributions' checksums. Each toolchain
   records `rustc -vV` and a content-tree SHA-256; native cargo-bin hashes are
   measured independently. Measurement requires exactly
   `rustc 1.100.0 (0ef1bc386 2026-11-09)` and
   `cargo 1.100.0 (3acc79c0c 2026-11-05)` for the new toolchain, its requested
   targets/components, and `cmake version 4.2.3`; it cannot replace requested
   versions with whatever was installed. Linux's sccache and cross-targets are not installed.
   Stable is an independent copy of the numbered release, with exactly its pinned
   subset of components, never an installation from the moving stable channel.

   The root agent LaunchDaemon is installed for subsequent job boots. `motorjob`
   is non-admin, has no login shell/password, and cannot write `/opt/motor/rustup`.
   Prep tools are exported as a standalone `tools/bin/{cargo,rustc,bun,git}` tree
   with Rust libraries and relocatable Git helpers beside them.

4. Review `<work>/candidate.json` (`<work>` is the path passed to `--work`), including source digests first acquired during
   bootstrap, all versions, component sets, `rustc -vV`, tree hashes, agent hash
   and prep-tool hashes. Replace `script/macos-runner/toolchain.json` with the
    reviewed candidate, change only `release_status` to `reviewed`, and commit it.
   After measurement, remove `1.100.0` from `PENDING_MACOS_TOOLCHAINS` in
   `parity.py` when committing the replacement lock. Keep that exception until
   the rebuilt image is measured; it lets the live image pass parity while the
   Linux images already carry 1.100.0. No component or Bun exceptions need removal.
   Do **not** re-provision the measured VM just to seal it: native rebuilds can
   produce different bytes. If provisioning failed midway, discard that builder
   and start measurement from a fresh restore instead of reusing partial scratch.

5. Seal that measured guest using the now-committed lock:

   ```sh
   python3 script/macos-runner/image.py seal \
     --work target/scratch/macos-image --guest motorbuild@GUEST_IP \
     --known-hosts "$GUEST_KNOWN_HOSTS"
   ```

   The seal uploads this commit's guest code, then the guest rechecks its
   installed tools against the reviewed pin lock (`toolchain.json`), removes the
   build account from the admin group and checks it is no longer a member, turns
   Remote Login off and verifies it is off, and only then deletes the account's
   `.ssh` and temporary sudoers entry, removes build sources, and acknowledges
   sealing before shutdown. Every step that can be refused runs while the build
   key and sudo still work, so a failure leaves the builder reachable for
   investigation. The
   host requires that acknowledgement even if SSH is disconnected
   by shutdown, and deletes its private/public build key. If Remote Login shutdown
   severs SSH before acknowledgement on a particular macOS release, sealing fails
   closed and requires operator investigation; it is not counted as success.

   The account itself stays, and the running guest can't lock it: Setup
   Assistant made it the Data volume's only Secure Token holder, and macOS
   refuses to delete that account or change its authentication authority, even
   as root (`eDSPermissionError`). Finalize disables it offline instead (next step).

6. After the image-build helper has exited, finalize. Every disk must be closed
   (`lsof` must prove this). Finalization mounts only the stopped image's APFS Data
   volume, disables the `motorbuild` account in its record (a non-login shell, a
   `;DisabledUser;` authority with its password hash deleted, the unusable `*`
   password marker, no admin membership), refuses unless the record then passes
   the locked-account check below, grants the installed
   guest agent macOS TCC access to network volumes
   and all files, then unmounts and detaches before hashing the bundle. The grants
   use the installed agent's code-signing requirement, not a build output: macOS
   treats virtiofs workspaces as network volumes and otherwise denies reads and
   directory listings, including for the agent's job children. The system TCC
   database is SIP-protected while the guest runs, so these grants are written
   offline. `image.json` records both services and the requirement, and the
   directory/files become read-only:

   ```sh
   sudo python3 script/macos-runner/image.py finalize \
     --work target/scratch/macos-image --image "$IMAGE"
   tar -xf target/scratch/macos-image/tools.tar -C target/scratch/macos-image
   ```

## Offline guest-agent update

Use `agent-swap` on a dedicated macOS VM host to update a stopped, sealed image
without restoring the IPSW or repeating Setup Assistant. Run as root so the
installed binary can retain `root:wheel` ownership. The destination must not
exist; its parent must already exist on an APFS volume supporting `cp -c` clones.
There is no full-copy fallback.

1. Build and sign the replacement agent from a known source commit. Review its
   SHA-256 and full source commit in a new toolchain manifest, changing **only**
   the `agent` record from the source image's installed lock. Commit that reviewed
   manifest in this repository; pending or uncommitted locks are rejected. The
   binary hash is checked before cloning and again on the installed replacement;
   its source-commit provenance remains the operator's responsibility.
2. Stop all VMs using the source, then run:

   ```sh
   sudo python3 script/macos-runner/image.py agent-swap \
     --from "$SEALED_IMAGE" --to "$UPDATED_IMAGE" \
     --agent "$NEW_MACAGENT" --manifest script/macos-runner/toolchain.json
   ```

   `image.json` is generated inside each sealed image directory, not stored in
   the repository. The recipe checks its complete file inventory, read-only modes
   and digests, and uses `lsof` to prove the source is stopped. It APFS-clones the
   four image files into a temporary sibling of the destination, mounts only the
   clone's Data volume writable, verifies the old installed pins, and atomically
   replaces `/opt/motor/bin/motor-macagent` in the same directory as `root:wheel`
   mode `0555`. It updates `/opt/motor/{toolchain.json,agent.sha256,sealed.json}`
   for the reviewed manifest, re-grants TCC using the replacement's installed
   code-signing requirement, unmounts/detaches, and recomputes the outer hashes.
   Only then is the read-only bundle published by rename. Failures leave no
   destination or staging bundle; the source bytes and modes are not changed.
3. Re-run installed-daemon qualification before deploying the new image. Portable
   tests cannot prove APFS cloning, code-signing/TCC behavior, or guest startup.

Agents speaking macOS protocol version 2 format and mount each job's build disk
at `target/` (see `docs/macos-runner.md`). The runner refuses an image whose agent
reports version 1, so install the runner and an image carrying a version 2 agent
together; an `agent-swap` is enough, as the seal steps are unchanged. The agent
uses only tools every macOS guest has: `/usr/sbin/diskutil`, `/usr/bin/plutil` and
`/sbin/mount_apfs`. After installing, run
`script/macos-runner/build-disk-e2e.sh` (below) once on the VM host.

### Build disk end-to-end check

`script/macos-runner/build-disk-e2e.sh <checkout under /Users> [<runner state root>]`
runs two jobs through the installed runner on a new, disposable Cargo checkout.
The first job checks that `target/` is an APFS volume mounted `nosuid,nodev`,
separate from the shared checkout and owned by the job account; it then builds
there and returns a file from `target/`, whose hash the script compares. The
second job checks that the same volume comes back: it finds the first job's
marker, and Cargo compiles nothing. Neither job may carry the reformatting
notice. With a state root, the script also checks that the kept
`targets/<key>.img` is exactly 128 GiB and sparse and that no old build folder
remains. It prints each check, keeps both jobs' records beside the checkout, and
exits non-zero at the first failure.

Earlier sealing code set a non-login shell and `*` password marker for
`motorbuild` but kept its password hash, enabled authority and admin
membership. Older sealed images may retain that account in this weaker state.
The swap never deletes or repairs that account: it accepts an absent account, or
checks the local account plist at
`/private/var/db/dslocal/nodes/Default/users/motorbuild.plist` offline for
`/usr/bin/false`, `passwd = ["*"]`, and no enabled authentication authority.
Every authority string must start with `;DisabledUser;`, or
both the authority list and `ShadowHashData` must be absent/empty. The password
marker alone is **not** proof that a ShadowHash password is disabled. The home
must be `/Users/motorbuild` with no `.ssh` contents or symlinks; unknown states
are refused with a reason. Direct admin membership (by short name or UUID) is
printed for operator review, not removed. A retained admin entry does not grant
login when password authorities are disabled and SSH keys are absent. This does
not claim to inspect effective network/directory-service group membership.

Tests: `OfflineImageTests` covers cloning, source preservation, replacement and
TCC metadata, refusal of enabled credentials and partial failures;
`ImageTests.test_sealing_refuses_if_build_account_survives_deletion` covers the
seal acknowledgement's account-deletion prerequisite.

## Host installation and update

Run from the operator account using sudo. A first install requires
`--operator-account "$OPERATOR_ACCOUNT"`, which must name the invoking `SUDO_USER`.
The installer records that account in
`/Library/Application Support/Motor/macos/install.json`; updates must come from
the recorded account and may omit the flag. To bind a different operator account,
uninstall first, then reinstall with an explicit `--operator-account`.
Supply the submitting client's SSH public key as one plain OpenSSH Ed25519 public
key line (not a CA or certificate):

```sh
sudo python3 script/macos-runner/install.py install \
  --operator-account "$OPERATOR_ACCOUNT" \
  --image "$IMAGE" --state-root /Users/motor/.motor --instance macos \
  --runner "$RUNNER" --vm-helper "$MACVM" \
  --tool-bundle target/scratch/macos-image/tools --runner-key "$RUNNER_IDENTITY_PUB"
# Restart the host yourself when prompted, then verify the logged-in runner:
sudo python3 script/macos-runner/install.py --check \
  --image "$IMAGE" --state-root /Users/motor/.motor --instance macos
```

Without `--data-volume`, state and image must remain on internal APFS storage.
For an external SSD, obtain its **Volume UUID** with
`diskutil info -plist "$DATA_MOUNT"` (not a disk, partition, or APFS container
UUID). It must be APFS,
report fixed rather than removable media, and have ownership enabled. If needed,
run `sudo diskutil enableOwnership "$DATA_MOUNT"`; ownership is required for the
runner's file isolation. Put both state and the sealed image beneath that mounted
volume, then install:

```sh
sudo python3 script/macos-runner/install.py install \
  --operator-account "$OPERATOR_ACCOUNT" --data-volume "$DATA_VOLUME_UUID" \
  --image "$IMAGE" --state-root "$DATA_MOUNT/motor-state" --instance macos \
  --runner "$RUNNER" --vm-helper "$MACVM" \
  --tool-bundle target/scratch/macos-image/tools --runner-key "$RUNNER_IDENTITY_PUB"
# Restart the host yourself when prompted, then check the recorded volume:
sudo python3 script/macos-runner/install.py --check \
  --image "$IMAGE" --state-root "$DATA_MOUNT/motor-state" --instance macos
```

macOS ties privacy approvals, such as a runner's access to an external data
volume, to the program's code signature. The installer normally re-signs the VM
helper ad hoc, and an ad-hoc signature changes with every build, so each update
asks again at the host's screen. To avoid that, sign `motor-runner` and
`motor-macvm` with one team identity before installing (the helper with
`virtualization.entitlements`) and pass `--signing-team TEAM_ID`. The installer
then keeps those signatures, refuses binaries signed by another team or ad hoc,
and records the team in `install.json`; updates inherit it, and changing it
requires uninstalling first. With a team signature, grant `motor-runner` Full Disk
Access once in System Settings → Privacy & Security; an administrator can do
this from their own session, and the grant survives updates signed by the same
team.

The selected Volume UUID is recorded in `install.json`; updates and `--check`
use it even when the flag is omitted. A different UUID (including adding one to
an internal-storage installation) requires uninstalling first. Installation sets
`AutomountDisksWithoutUserLogin` to true in
`/Library/Preferences/SystemConfiguration/autodiskmount`, allowing external
disks to mount at boot before a user logs in. A root-only
`autodiskmount-backup.plist` beside `install.json` preserves the previous value
or its absence across updates; uninstall restores it. Do not remove that backup
while the installation is active.

For a selected data volume, the LaunchAgent's standard output and error go to
`/Users/motor/Library/Logs/motor-runner/` on the internal disk: launchd opens those
files itself before the runner starts, and macOS privacy rules refuse launchd's
open on an external volume even when the runner has Full Disk Access. The
LaunchAgent's `WorkingDirectory` is the absolute
state root. `Runner::open` can recursively create a missing state root, so startup
must stop before that call when the disk is missing, rather than create state on
the internal disk. This relies on launchd refusing to spawn when its working
directory is missing; the portable tests pin the path configuration, not that
native launchd behavior. Verify refusal on a real host with the disk unmounted
before relying on the missing-disk protection.

The installer creates `motor` if absent and refuses an existing admin motor
account. It links `/Users/motor/.cargo/bin/motor-runner` to the installed runner,
because ck-motor's SSH session runs `~/.cargo/bin/motor-runner connect` as motor,
and `--check` refuses a missing or different link. It installs the runner/helper under `/Library/Application Support/Motor`,
**re-signs the installed VM-creating helper on every install/update**, and verifies
its virtualization entitlement. Prep binaries and libraries are copied under
`<state_root>/tools`, with all four executable hashes checked before and after
copy. The root-owned, mode-0644 LaunchAgent at
`/Users/motor/Library/LaunchAgents/io.cortexkit.motor.macos.plist` runs in motor's
GUI login session, with `LimitLoadToSessionType = Aqua` and no `UserName` or
`GroupName`. Its directory belongs to motor. It explicitly selects
`--backend macos-vm`, without the development override. The runtime TOML uses the
runner's existing schema, with absolute image/helper paths and four pinned prep
tools; no image provenance fields are added to that schema.

**FileVault must be off.** Installation refuses otherwise: automatic login needs
an unencrypted login path. The dedicated, non-admin `motor` account becomes the
host's auto-login user, replacing the provider's previous auto-login choice. This
means anyone with physical/console access after a restart enters that account;
use only a host where that policy is acceptable. The installer generates a random
account password and keeps it in `/var/root/motor-autologin-password` (root-only,
0600). It writes Apple's XOR-obfuscated `/etc/kcpassword` (also root-only, 0600;
**not encryption**) and sets loginwindow's `autoLoginUser` to `motor`. Before
changing login settings, it saves the previous user, obfuscated password and login
keychain in `/var/root/motor-autologin-backup.plist` (root-only, 0600).

On first setup it recreates motor's `~/Library/Keychains/login.keychain-db` with
the account password and selects it as the user's default keychain. Updates reuse
the saved password and preserve that keychain, including VM Secure Enclave keys.
Every install/update unlocks that keychain as motor and disables idle-timeout and
sleep locking, so VM Secure Enclave signing still works after quiet periods. Do
not delete the password or backup while the installation is active.

Apple's Virtualization framework creates a Secure Enclave key at VM start and
requires a logged-in Aqua session. A system LaunchDaemon with `UserName = motor`
is not such a session: VM starts can fail with `errSecInteractionNotAllowed`
(-25308), reported as “The virtual machine encountered a security error.” The
installer stops/removes the old `/Library/LaunchDaemons` runner job and uses
`launchctl bootstrap gui/<uid> <agent-plist>` and
`launchctl bootout gui/<uid>/io.cortexkit.motor.macos` instead. If motor has no
GUI session, installation leaves the agent for the next login rather than
bootstrapping it into a different domain. **Restart is required when the auto-login
user changes; the installer never restarts the host itself.** Keep motor logged in
at the console, and do not switch the auto-login user back while serving jobs.

The marked sshd block applies only to `motor` and accepts only the pinned key in
the root-owned `/etc/ssh/motor/authorized_keys`, with no user CA. When the vault
key is rotated, rerun the install with the new public key. `sshd -t` validates edits; `--check`
also checks `sshd -T -C user=motor,...`, not merely a matching comment. Existing
provider SSH settings are preserved. `sudo python3 script/macos-runner/install.py
uninstall` stops the runner in both launchd domains, removes both runner plists
and the marked SSH block, restores the backed-up auto-login settings and login
keychain/default, and removes the generated password and backup. The old account
password is not recoverable: uninstall revokes the saved motor credential by
rotating to a fresh random password and discarding it, without disabling the
account. If the previous auto-login user was already motor (for example, a manual
setup), uninstall instead turns auto-login off and removes `/etc/kcpassword`:
restoring motor auto-login with a revoked password would leave a broken login.
The operator can re-enable auto-login for their own account. Uninstall retains
the account, image, pinned tools and runner state;
it is not a destructive data/account cleanup command. It does remove motor's
data-protection keychain folder (`~/Library/Keychains/<host UUID>/`), which stays
bound to the revoked password and would make a later install's keychain unlock
fail. Restart afterwards to leave the motor login session, and before installing
again: macOS keeps motor's old keychain binding in memory until a restart.
Repeated uninstall is safe.

`--check` requires internal APFS state and image volumes unless `install.json`
records a data-volume UUID. With a recorded UUID, both resolved paths must be on
that mounted APFS volume, with ownership enabled, fixed media, and
`AutomountDisksWithoutUserLogin` true; a missing condition is a refusal with a
remedy. It also verifies the external-volume LaunchAgent working directory.
It refuses missing entitlement,
wrong effective SSH CA/principals, a non-motor or non-0600 socket, changed tool
bytes, less than 20 GiB free, or a plist without the VM backend/with a development
flag. It also verifies the sealed-image inventory and installed lock, mounts the
image read-only to compare the agent's current code-signing requirement with its
recorded TCC grant, and requires the login keychain's `no-timeout` setting. A missing
file or failed native probe is a refusal, not a skipped check. It verifies the
exact LaunchAgent configuration, auto-login user, FileVault-off status and a
running job in `gui/<uid>`. Before motor's Aqua session exists it fails with a
restart instruction, even if the runner socket has not been created yet.
`InstallerTests` and `AutoLoginTests` in `test_macos_runner.py` cover these
contracts with mocked native commands; they never change the test host's users.

## Verification and qualification

Portable contracts:

```sh
python3 script/macos-runner/test_macos_runner.py InstallerTests AutoLoginTests
python3 -m unittest -v script/macos-runner/test_macos_runner.py script/macos-runner/test_qualification.py script/runner/test_toolchain_parity.py
```

Inside a real job VM, as `motorjob`, from the repository snapshot:

```sh
python3 script/runner/toolchain-parity.py --check --role macos --job \
  --manifest script/macos-runner/toolchain.json
```

This also loads the **build-server** manifest from `script/runner/toolchain.json`.
Changing its rustc strings without updating the macOS pins fails validation. The
installed guest lock at `/opt/motor/toolchain.json` is an independent record of
what was sealed, so changing a snapshot pin alone fails too. The macOS role does
not query apt/dpkg, Linux mounts, compile wrappers or Linux coreutils. Under
`--job`, every installed rustup directory/file must be unwritable by the job uid.
TMPDIR is a dynamic `motor-job-*` directory; all other pinned environment values
are exact, including an unset `CARGO_TARGET_DIR`. Linux/Boat retain their existing
capture/check paths and their manifest is unchanged.

Portable checks and Swift typechecking do not qualify a real VM. Run all installed
`macos_qualification_*` lanes documented in `docs/macos-runner.md`, including
Aqua-session runner startup, libproc identity, actual APFS clones on the selected storage and guest mounts,
vsock/nonce isolation, key deletion and Remote Login shutdown. No such privileged
or live-VM qualification is claimed by this recipe's portable tests.

### Operator evidence tools

Install the reviewed `ck-motor` as well as the runner/helper. `ck motor run
--record-file <new-path>` saves Accepted/Started/Output/Terminal as 0600 JSONL,
separate from CLI diagnostic stderr. These are typed protocol records, not the
route envelope containing the reattach secret; Output bytes remain base64 and
can themselves be sensitive. The actual `Accepted.platform` is retained.

On an idle dedicated instance, set `MOTOR_MACOS_QUALIFICATION_EVIDENCE` to a
persistent writable directory outside build/cache trees. The ignored native
lanes retain per-job protocol records, CLI streams/exits, observed ownership
records and live helper VM requests there. The concurrent lane also requires
`MOTOR_MACOS_QUALIFICATION_WORKSPACE_SECOND`: a separate canonical Git root,
not another cwd inside the first checkout. Only the two Accepted job IDs' live
service identities count toward overlap; ambient VMs cannot satisfy it.

`guest_capture.py` wraps each concurrent build once, captures its raw streams,
wait status and build duration, then waits for host collection before exiting.
The host copies those files out of the job tree before acknowledging.
`python3 script/macos-runner/qualification.py <job-evidence-directory>` checks
independent guest bytes/status against decoded Output, Terminal and CLI exit.
It tolerates identical replay, not output gaps, reordering or changed replay.
`test_qualification.py` tests both the independent capture handshake and negative
stream/status comparisons; these are not live VM proofs.

Create reviewed disposable fixtures with
`python3 script/macos-runner/fixtures.py <new-directory-under-/Users>`.
The generator performs no fetch, refuses an existing destination, and creates
committed workspace/sibling repositories with a locked Darwin-arm64 esbuild
package and both Bun `file:../sibling` and Cargo `path="../sibling"` dependencies.
Use their paths for the Bun lane with cold dedicated caches; `bun verify.js`
uses both the JS sibling and the native package, while Cargo tests the Rust
sibling. Locks are tracked and hashed before/after the guest build.

The generator also creates private-good/private-missing repositories with an
unreachable loopback HTTPS source, two distinct locked commits, and a separate
client Cargo cache holding only the positive commit. Configure that cache as
`CARGO_HOME` in the **installed private ck-motor service** before the private lane;
a test-shell variable does not configure the service. Set the lane's
`MOTOR_MACOS_QUALIFICATION_PRIVATE_GOOD`, `_PRIVATE_MISSING` and `_PRIVATE_URL`
from `fixtures.json`. Retain an owner-controlled host network capture with a
labelled control connection showing it records traffic; verify prep did not
contact that URL. The negative
job must refuse with its source named and no Started record. The guest's absent
NIC alone cannot prove host prep stayed offline. Run host tests as `motor` with
read access to the configured state root's ownership records and job trees.
Provision the installed private ck-motor service with vault SSH signing
access for the `motor` runner route. That service setup, cold-cache provisioning
and live network observation remain owner work; never open state permissions or
evict shared live caches to make a lane start.

## Optional outbound monitor metrics

Metrics are off on first install unless enabled. Add `--metrics-endpoint https://metrics.example
--metrics-host-id monitor-1` to the normal install command to enable signed,
outbound-only monitor samples every 60 seconds. Supply both parameters or
neither: the endpoint must be an HTTPS origin without credentials, a path, query
or fragment; the id must match `[A-Za-z0-9._-]{1,64}`. Reinstalling without either
parameter preserves the installed metrics settings; supplying both replaces
them. Use `metrics-config --disable-metrics` to turn metrics off.

Once installed, print (or create once) the stable public key as 64 hex characters
without loading or starting the LaunchAgent:

```sh
sudo python3 script/macos-runner/install.py metrics-key
```

The command reads the installed LaunchAgent's runner binary, `--instance` and
`--state-root` and runs `motor-runner metrics-key` as `motor`, keeping the signing
key under the same private state folder as the service:
`STATE_ROOT/INSTANCE/monitor/host-key`. Register only the public key with the
receiver.

For an existing installation, stage only the metrics flags for the next planned
LaunchAgent reload or host restart, without stopping or starting it or replacing binaries:

```sh
sudo python3 script/macos-runner/install.py metrics-config \
  --metrics-endpoint https://metrics.example --metrics-host-id monitor-1
```

Use `metrics-config --disable-metrics` to stage removal. The normal `--check`
command reports the installed metrics flags, validates them, and includes them
in its LaunchAgent comparison; an optional metrics pair on `--check` also checks
that the installed pair matches the requested one. A staged setting is not a
claim that the currently running process has restarted with it.
Reload at the planned maintenance window by booting out and bootstrapping the
LaunchAgent through the normal install/restart procedure, or restart the host;
`launchctl kickstart` alone does not reload edited plist arguments.
