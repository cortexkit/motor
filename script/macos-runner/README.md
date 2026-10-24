# macOS VM image and host installer

This recipe uses the repository's `motor-macvm image-build` command, which calls
`VZMacOSInstaller` for restore and adds graphics and NAT **only** in image-build
mode. Production jobs use neither networking nor graphics. No third-party VM
manager is used.

## Pin-lock status: not deployable yet

`toolchain.json` is deliberately a **pending measurement** lock. The restore pin
is measured: macOS 27.0.1, build **26A434**, 26,637,307,067 bytes, SHA-256
`2f016638293c3e641b8b25391a76fbc16563b3711915a5551cf8aa0f5598a5c1`.
The operator supplied this hash from the restore image and checked it against
Apple's checksum on download. `observed_host` describes that measurement's
host, not a required guest image version. Obtain the exact named IPSW from Apple
or the operator's retained copy; `restore` checks its size and hash before use.

Rust versions, components and default are derived from the read-only build-server
manifest. GNU bash 5.2.37 is built from its pinned GNU archive (the source hash is
also published in Buildroot's `package/bash/bash.hash`). Git uses the server's
pinned kernel.org source release. The agent source is pinned to the full merged
Motor commit in the `agent.commit` value in `toolchain.json`; `git archive` exports that commit, not the current
working tree. The guest build records the installed agent's SHA-256.

The CLT receipt version and native binary hashes cannot be inferred from Linux
artifacts. They remain explicitly pending, never fabricated. `--measure` is the
only path allowed to bootstrap pending inputs. It writes a **candidate**, does not
mark it reviewed, and cannot seal or install an image. Normal provisioning,
sealing, installation and parity checking fail until every pending field is
filled and `release_status` is `reviewed`. Sealing additionally requires the lock
to be tracked and unchanged against HEAD. Review and commit the measured lock
before deploying it. Re-run measurement and reseal after any agent/protocol or
pin change; do not change only an artifact hash to make a check green.

## Dedicated-host operator procedure

Use a dedicated VM-building session on the rented Mac, not a job VM or an
operator's unrelated workstation. Host prerequisites: Apple Silicon, internal
APFS storage, Command Line Tools (including Swift and Python 3.9+), the native
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
   enable guest Remote Login, authorize only `build_key.pub` for that account,
   and temporarily allow its passwordless sudo in `/etc/sudoers.d/motorbuild`.
   Obtain the guest sshd host key from its console and put it in a dedicated
   known-hosts file; do not use an unchecked `ssh-keyscan` result as trust.
   Shut down after restore if necessary and use `image.py boot` (same arguments
   except `--ipsw`) to resume the **same** builder image in another terminal.

3. In the guest console inspect `softwareupdate --list` for the exact CLT label.
   Provision over SSH with strict host-key checking:

   ```sh
   python3 script/macos-runner/image.py provision --measure \
     --work target/scratch/macos-image --guest motorbuild@GUEST_IP \
     --known-hosts "$GUEST_KNOWN_HOSTS" --clt-label "$CLT_LABEL"
   ```

   The bootstrap installs that exact softwareupdate item. Apple authenticates the
   update; the guest checks the CLT receipt with `pkgutil --pkg-info-plist` and
   verifies the installed clang's Apple code signature. There is no invented CLT
   package-file hash: softwareupdate does not retain a package here.

   GitHub payloads (Bun and pinned Cargo source archives) use six parallel HTTP
   ranges, verify response ranges/lengths, then verify the complete SHA-256.
   A server that ignores ranges is refused, not silently downloaded serially.
   Use a hash-identical mirror URL in a reviewed source pin if range service is
   unavailable. Rustup verifies the Rust distributions' checksums. Each toolchain
   records `rustc -vV` and a content-tree SHA-256; native cargo-bin hashes are
   measured independently. Linux's sccache and cross-targets are not installed.
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
   Do **not** re-provision the measured VM just to seal it: native rebuilds can
   produce different bytes. If provisioning failed midway, discard that builder
   and start measurement from a fresh restore instead of reusing partial scratch.

5. Seal that measured guest using the now-committed lock:

   ```sh
   python3 script/macos-runner/image.py seal \
     --work target/scratch/macos-image --guest motorbuild@GUEST_IP \
     --known-hosts "$GUEST_KNOWN_HOSTS"
   ```

   The guest rechecks the installed pins, turns Remote Login off, verifies it is
   off, deletes the build account's `.ssh` and temporary sudoers entry, disables
   its shell/password, removes build sources, and acknowledges sealing before
   shutdown. The host requires that acknowledgement even if SSH is disconnected
   by shutdown, and deletes its private/public build key. If Remote Login shutdown
   severs SSH before acknowledgement on a particular macOS release, sealing fails
   closed and requires operator investigation; it is not counted as success.

6. After the image-build helper has exited, finalize. Every disk must be closed
   (`lsof` must prove this). This hashes the bundle, records the agent hash and
   makes the directory/files read-only:

   ```sh
   python3 script/macos-runner/image.py finalize \
     --work target/scratch/macos-image --image "$IMAGE"
   tar -xf target/scratch/macos-image/tools.tar -C target/scratch/macos-image
   ```

## Host installation and update

Run once as the provider's `rentamac` account using sudo; the same command updates
an installation. Supply the vault's **public** CA, never its private key:

```sh
sudo python3 script/macos-runner/install.py install \
  --image "$IMAGE" --state-root /Users/motor/.motor --instance macos \
  --runner "$RUNNER" --vm-helper "$MACVM" \
  --tool-bundle target/scratch/macos-image/tools --vault-ca "$VAULT_CA_PUBLIC_KEY"
sudo python3 script/macos-runner/install.py --check \
  --image "$IMAGE" --state-root /Users/motor/.motor --instance macos
```

The installer creates `motor` if absent and refuses an existing admin motor
account. It installs the runner/helper under `/Library/Application Support/Motor`,
**re-signs the installed VM-creating helper on every install/update**, and verifies
its virtualization entitlement. Prep binaries and libraries are copied under
`<state_root>/tools`, with all four executable hashes checked before and after
copy. The root-owned LaunchDaemon runs as `motor` and explicitly selects
`--backend macos-vm`, without the development override. The runtime TOML uses the
runner's existing schema, with absolute image/helper paths and four pinned prep
tools; no image provenance fields are added to that schema.

The marked sshd block applies only to `motor`, trusts the installed vault CA and
requires the `motor` authorized principal. `sshd -t` validates edits; `--check`
also checks `sshd -T -C user=motor,...`, not merely a matching comment. Existing
provider SSH settings are preserved. `install.py uninstall` removes **only** the
marked block, preserving unrelated sshd content, accounts, service and data.
Stop the daemon explicitly before retiring the host; uninstall is deliberately
not a destructive data/account cleanup command.

`--check` refuses external/non-APFS state or image volumes, missing entitlement,
wrong effective SSH CA/principals, a non-motor or non-0600 socket, changed tool
bytes, less than 20 GiB free, or a plist without the VM backend/with a development
flag. It also verifies the sealed-image inventory and installed lock. A missing
file or failed native probe is a refusal, not a skipped check.

## Verification and qualification

Portable contracts:

```sh
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
root-daemon startup, libproc identity, actual internal-disk clones and guest mounts,
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
