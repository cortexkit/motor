# Native offline Windows runner release

This bundle is for a Windows 11 Pro Hyper-V host. It contains the native
`motor-runner.exe`, a `hyperv-runner` agent (hvsock, admitting only the Hyper-V
host partition's peer VM ID, immutable nonce DVD), and the pinned **prebuilt** Rust fetch-only toolkit. The toolkit is
not rustup and is never used to build a job on the host. Jobs build only inside
fresh offline Generation 2 VMs. Git for Windows, Windows OpenSSH Server and
Python are host prerequisites. Installation copies the pinned prebuilt
Cargo/rustc components that Cargo uses to fetch and query target metadata;
it does not bootstrap rustup, modify a user toolchain, or compile artifacts on
the host.

## Security and qualification boundary

- The existing runner wire protocol and guest conversation are shared with the
  Linux QEMU runner. SSH uses the installed binary's `connect` bridge, through a
  local-only named pipe whose DACL names SYSTEM, Administrators and `motor-ssh`.
  Before sending job data, the bridge requires the kernel-reported pipe-server
  PID to match the running `MotorRunner` service's SCM process ID.
- `motor-ssh` is a dedicated non-admin account with exactly one supplied vault
  Ed25519 public key. An owned OpenSSH `Match User` block forces the bridge and
  disables password authentication, PTY, agent/TCP/X11 forwarding. Installation
  stores the key at `C:\ProgramData\MotorRunner\ssh\authorized_keys`; the file
  and directory are Administrators-owned, with protected ACLs granting only
  SYSTEM and Administrators. The Match block uses
  `__PROGRAMDATA__/MotorRunner/ssh/authorized_keys`, so a temporary Windows
  logon profile cannot change key lookup. No installer step needs a `motor-ssh`
  profile: the forced command, binary access, pipe ACL and service-status grant
  use absolute paths or the account SID. Uninstall removes the key directory
  and still cleans recorded profiles from older installations.
  `ssh-key-fixed-directory-exact-private-acl-and-idempotent-repair` and
  `ssh-cleanup-old-hand-fixed-and-profile-free-layouts-is-idempotent` in
  `test_scripts.ps1` exercise both layouts. Installation checks `sshd` syntax
  before restart, restores the original configuration on validation failure,
  and verifies effective public-key-only authentication,
  password, TCP/agent forwarding, PTY and forced-command settings. It also
  configures X11 forwarding off where Windows OpenSSH supports that option. It
  prints sshd's public host key for the client pin. No ambient client SSH configuration is required.
 - VM management is a boot-start SCM service running as SYSTEM. Production preparation
   does not run Cargo or Rustup on the Ally host. Without a root `Cargo.lock`,
   jobs skip local-registry preparation. Otherwise the runner reads the copied Cargo.lock,
   downloads only allowlisted crates.io index records and `.crate` archives, verifies
   each locked checksum before publication, and maintains a SYSTEM/Administrators-only
   local-registry cache. Git sources are assembled from the job's validated bundles.
   Each VM receives a per-job local registry and Cargo-home source replacement; Cargo
   runs only inside the offline VM. The `OfflineCargoOnly` and `LocalRegistryOnly`
   qualification lanes use pinned Cargo as SYSTEM in disposable fixtures; they do not
   change production fetching. Runner state and cache remain under the private root ACL.
- Each job gets a fresh differencing OS disk and nonce. A tiny read-only DVD
  delivers boot metadata without mounting a job disk on the host. It is not a
  host filesystem share or a live guest/host service. The real workspace cache
  disk is separately attached at SCSI LUN 1. Selection requires exactly two
  disks: the boot/system OS disk at LUN 0 and a non-system cache at LUN 1, with
  the cache's device serial captured and rechecked. Dirty caches are discarded;
  restarting the native runner service conservatively discards all old caches.
  The `MOTORNONCE` DVD contains root `nonce.txt`: exactly 32 hexadecimal bytes,
  UTF-8 without BOM or newline. The host protocol expects the identical compact
  value; job/VM UUIDs remain hyphenated. Optional `peer-targets.json` is a separate
  UUID array consumed by the image's peer-qualification startup task. Its canonical
  UUID strings are joined into a flat JSON array explicitly: PowerShell 5.1 can
  otherwise nest/decorate arrays with `value`/`Count` during JSON conversion.
- Service and qualification startup reuse the SYSTEM/Administrators-only parent
  record when its full file identity matches. Only a missing, unreadable or
  changed record triggers hashing, with a separate 15-minute validation deadline
  and hash-duration evidence. Each runner process holds a read-sharing,
  write/delete-denying parent handle from before verification until its lifetime
  ends, including service idle time and qualification between control calls.
  Prepare and Run also hold their own guards and never refresh a mismatch.
  ChangeTime is deliberately not an identity field: Hyper-V rewrites identical
  security descriptors at VM creation/removal. File ID, size, LastWrite, read-only
  and owner/ACL must still match. The exact Hyper-V VM Bus Provider capability SID
  receives read-only parent access alongside the VM group; no other grant is added.
   Restart the MotorRunner service to re-verify a changed parent. Startup verification and Windows jobs both rely on the sealed parent's identity record.
   At install, the guest-agent SHA-256 is copied from the sealed image's
   `build\provision\winagent.sha256` provisioning record into `vm.toml`. Startup
   compares it with the release's `bin\motor-winagent.exe`; if missing or different,
   health reports `windows_guest_agent.ready=false`, new Windows runs are refused
   before start with both hashes, and operator readiness fails. The host bundle
   does not replace the agent inside an already sealed parent. Provision a fresh
   image from the bundle before upgrading; the Linux `agent-swap.sh` only handles
   qcow2 images and does not upgrade the Hyper-V runner's sealed VHDX.
   SYSTEM and Administrators are trusted; while no runner process holds a guard,
   they can rewrite equal-length content and restore LastWrite without detection.
   That service-stopped maintenance window is accepted, not prevented by metadata.
- No job NIC, COM mapping, optional integration service or Enhanced Session.
  Host isolation is checked before boot, every 250 ms while running, and at
  shutdown. The creation-time VM configuration checks and later samples are
  evidence, not a claim that a privileged operator cannot attach a device between samples. Additional
  host hvsock registrations fail preflight for review; none are silently removed.
- Shipping capacity is **one 4-vCPU / 8-GiB job**. The native resources and
  scheduler support at most two, but do not raise `concurrent_jobs` to 2 until a
  separate two-real-job native concurrency qualification has passed and its
  evidence has been reviewed. The two-VM peer test is not that qualification.
- The service drains when AC disappears and holds a system-required execution
  request while on AC. Installation saves/disables automatic AC sleep and
  hibernation and saves/disables Enhanced Session. Uninstall restores those
  settings; it does not disable Hyper-V. User-initiated sleep remains Windows'
  operator control.

A passing portable suite is **not** native qualification. Leave fleet routing
disabled until the complete operator evidence has been reviewed, including the
real loopback/foreign-VM refusal logs and mid-frame disconnect byte counts.

Native removal always attempts a stopped, read-only guest-log harvest before
deleting the VM or child disk. Protocol-complete jobs use a lightweight agent/peer
log copy; failures, disconnect qualification and orphan recovery collect the full
known log/marker set and agent-filtered System/Application events. Collection
errors and absent disks/logs are explicit evidence, not a reason to skip cleanup.
Qualification copies that host evidence to its external folder on failure too.
Harvest asserts the VM's OS attachment, requested differencing child, mounted VHD
disk number and reverse partition/drive-letter association. The resulting
`job-disk-binding.json` and `mounted-disk-identity.json` show the actual chain and
volume identities; lookup failures are distinct from an absent guest file.

The disconnect qualification alone records prepare/connect/Hello/write/drop
milestones and observes a bounded 60-second diagnostic window before forced power-off.
The VM worker's process counters are not per-disk durability and do not shorten
that window: a quiet process can still have guest/kernel storage work pending. No
running guest filesystem is mounted. The first safe log observation is recorded
after power-off relative to stream drop, not claimed as the guest's write time.
The exact terminal-read/read-count assertion remains required and is bound to the
current nonce: the final successful read total must equal the host's partial write,
and the next read must end as zero-byte EOF or terminal hvsock10053/10054. No partial
frame may be processed and no successful reply bytes may follow its reception.
The host deliberately closes both directions rather than promising an SD_SEND
half-close; failed write attempts after that close are not successful replies.
Production does not wait on or depend on guest log persistence.
The Hyper-V guest diagnostics are selected at build time (`hyperv-runner` enables
`hyperv-hvsock` and `hyperv-spike`); that profile logs on every boot, not only when
a nonce-disc flag is present. Host identity JSON is emitted without a UTF-8 BOM;
qualification readers also accept older PowerShell 5.1 BOM-prefixed artifacts.

To measure the offline cache inputs Cargo needs, use the SYSTEM-only diagnostic lane.
After installing a bundle built from the reviewed source commit, run this from
elevated PowerShell 5.1:

```powershell
$python = (Get-Command python.exe -ErrorAction Stop).Source
$evidence = 'C:\motor-runner-evidence\offline-cargo-' + (Get-Date -Format 'yyyyMMdd-HHmmss')
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\MotorRunner\bin\run-system.ps1 `
  -Python $python -Evidence $evidence -OfflineCargoOnly
if ($LASTEXITCODE -ne 0) { throw "Offline Cargo diagnostic failed; preserve $evidence" }
```

Pinned Cargo first attempts to fetch the locked `cfg-if 1.0.4` fixture as SYSTEM.
The report records the crate archive path and checksum and the registry-configuration
contents. For each observed `cfg-if` sparse-index entry, it records a read error
if the entry cannot be opened; otherwise it records the full size and SHA-256, the
first 64 KiB of original entry bytes as hex (up to 128 KiB of hex text), and an
escaped rendering of the first 96 original bytes. The report marks entries whose
hex capture is truncated. It then runs pinned Cargo offline as SYSTEM in fresh
homes containing (1) only the `.crate` archive, (2) that archive plus the observed
registry configuration but no `.cache` entry, and (3) the full Cargo-generated
cache. Each cell records Cargo's exit status and output. This distinguishes
whether the lockfile and crate archive suffice and, if not, which actual index
files Cargo requires; no sparse-cache bytes are synthesized by the runner. No
restricted Cargo process is launched. The diagnostic is evidence collection only
and does not change production fetching. Inspect the copied
`prep-offline-cargo-*.json` report; leave the service stopped for review.

Before using local Cargo registry source replacement in production, run its
SYSTEM-only proof lane from elevated PowerShell 5.1:

```powershell
$python = (Get-Command python.exe -ErrorAction Stop).Source
$evidence = 'C:\motor-runner-evidence\local-registry-' + (Get-Date -Format 'yyyyMMdd-HHmmss')
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\MotorRunner\bin\run-system.ps1 `
  -Python $python -Evidence $evidence -LocalRegistryOnly
if ($LASTEXITCODE -ne 0) { throw "Local registry proof failed; preserve $evidence" }
```

This disposable diagnostic directly fetches the index config, raw `cfg-if` index
record, and checksum-pinned `.crate` from the allowlisted HTTPS hosts, refusing
redirects and responses above their size caps. It builds a protected persistent
cache and assembles two fresh job registries; the second job must make zero
network requests. Each Cargo home starts with a runner-written source-replacement
config and local-registry data, but no Cargo sparse cache. Pinned Cargo runs
`fetch --locked --offline` and `build --locked --offline`; the report records both
results and confirms Cargo.lock stayed byte-identical. A one-byte archive mutation
must be refused before writing by the runner's checksum gate, and Cargo must reject
the same changed archive when that gate is bypassed. A fixture repository
`.cargo/config.toml` selects an empty source to show whether project config overrides
the Cargo-home replacement; repository replacements are allowed, and Cargo remains
offline and verifies Cargo.lock checksums. This is evidence collection only; it does
not alter production fetching. Inspect `$evidence\host-evidence\prep-local-registry-*.json`
and leave the service stopped for review.


Peer qualification starts the victim alone and records its VM-matched Hyper-V
Worker/Admin OS-boot event 18601, then waits at least 20 seconds before starting
the attacker. There is deliberately no readiness dial: the unchanged guest agent
closes its single listener on its first host accept. The attacker is stopped and
read-only harvested first; its completed proof must name the current victim VM.
Only then does the host connect and validate the victim's Hello version and nonce.
After a full 60-second qualification-only log-settle window, the victim log must
show one current-nonce listener, native loopback refusal, and exactly one parent
accept. A connected attacker additionally requires its native refusal in that log;
a failed sibling connect is accepted only with the boot/margin, completed proof,
and final host Hello evidence. This proves that the sibling attempt did not consume
or reach the single listener, not that every sibling transport failure is a native
agent refusal or that a particular Hyper-V routing policy is universal. The fixed
boot margin assumes normal service startup; restarted/multiple listeners fail.
`peer-timeline.json`, boot-event XML and `peer-host-hello.json` retain host-side
ordering and the actual received frame; guest clocks are never subtracted from
host clocks. Production jobs acquire no new delay and the sealed image is unchanged.
The boot/service margin and both peer log-settle windows use absolute deadlines
on the same monotonic Stopwatch that records their evidence. The Hello timestamp
is captured immediately after validation, and its settle deadline is anchored to
that recorded value; a sleep request or early timer wake is not elapsed-time proof.
The OS-boot matcher reads the Chipset event's namespaced `UserData/VmlEventLog/VmId`
as a GUID, allowing uppercase spelling and event timestamps up to two seconds
before the recorded host start. Poll queries and candidate XML are retained even on timeout. Peer
failures retain the original phase/error; each monitor gets a disposition JSON
and stderr reason, including parent-requested teardown kills, so cleanup cannot
replace a boot-gate error with an unexplained nonzero exit.
Before real-job qualification, `qualify.ps1` restarts MotorRunner and runs a
180-second readiness check. SCM `Running` is not sufficient: the service reports
it before parent validation and pipe creation. Readiness requires a Hello with
the expected protocol version and Windows platform through the service-owned
bridge; that listener is bound only after startup validation and reconciliation.
The `qualify-jobs.py --wait-ready` mode submits no jobs and retains SCM states,
bridge errors and the successful Hello observation. Stopped services, invalid pipe
ownership and wrong Hello data fail explicitly; missing pipes during startup are
retried within the readiness limit. Any qualification error still intentionally
stops the service, so a post-failure `Stopped` observation is not itself a crash.
The real-job fixture bundle advertises only a temporary
`refs/ck-motor/<uuid>/snapshot`, matching the production snapshot transport.
It never uses `--all`, which would also advertise forbidden branch/HEAD refs.
The fixture uses the same commit for its snapshot and integration tip; its
temporary ref is deleted after bundle creation, including on failure.

## Build on Linux, not on the Windows host

From a source checkout, only after approving Microsoft's licence terms:

```sh
bash script/windows-runner/install-cross.sh --accept-microsoft-terms
python3 script/windows-runner/fetch-prep.py
# Disconnect build network after the fetch steps.
bash script/windows-runner/build-release.sh
python3 script/windows-runner/bundle.py
```

`script/runner/toolchain.json` pins cargo-xwin, VS channel build and package
manifest content, exact CRT/SDK components and the private Microsoft payload
subtree hash (`cache/xwin`, retaining `xwin/` in relative names). Generated
`clang-cl`/CMake host wrappers outside that subtree are not SDK payload.
Only the generated `xwin/DONE` marker is normalized: its LF-separated lines are
sorted before hashing because cargo-xwin writes an unordered payload set. Line
content and multiplicity still matter; every other file and symlink is exact.
The cross tool/cache install stays under the source checkout's
`target/windows-cross`, with no host-global installation. xwin downloads Microsoft's MSVC CRT
and Windows SDK headers/import libraries under Microsoft's Build Tools/SDK
terms, the same terms accepted by the Windows job image's unattended install.
They are used privately to build our own software and **never redistributed**.
Cargo-xwin's MIT licence does not relicense Microsoft's components. The bundle
contains our binaries and the separately licensed Rust toolkit, not the SDK,
CRT cache, MSVC tools, source checkout, secrets or signing keys.

Our binaries are currently unsigned. Smart App Control must already permit
these binaries; no script disables it or changes WDAC. Signing remains strongly
recommended for publisher provenance and future application-control policy.
Before installation, verify that the host's application-control policy permits
these unsigned binaries; scripts do not change Smart App Control or WDAC.

## Operator sequence (elevated Windows PowerShell 5.1)

Run `install.ps1` from an elevated administrator shell, not a SYSTEM scheduled
task. Changing the parent file's owner requires that install context; the
installer rejects SYSTEM before creating or changing installation state.
`run-system.ps1` is for qualification only, not installation. Native commands are
judged by their process exit code; captured stderr and exit status are logged in
`C:\ProgramData\MotorRunner\installation-native.jsonl`.

First compare the ZIP's SHA-256 with the checksum supplied with the release,
then extract it to a fresh source directory, and run the parser/policy and native parent-identity fixtures **before any
installation or VM mutation**:

```powershell
$src = 'C:\motor-windows-release'
Expand-Archive C:\Transfer\motor-windows-release.zip -DestinationPath $src
Set-Location $src
powershell.exe -NoProfile -ExecutionPolicy Bypass -File script\windows-runner\test_scripts.ps1
if ($LASTEXITCODE -ne 0) { throw 'Portable PowerShell checks failed' }
```

Build a **fresh** sealed parent using the existing Hyper-V provisioning mode.
Use verified Windows Server 2022 installation media; Windows 11 Pro's host
licence does not itself license Server guests. Evaluation media may be used
for qualification only, never silently accepted as a permanent fleet image.
Do not pass `-LicenseAcknowledged` below until the guest's licence is settled.
No host-side Rust build occurs; the agent and peer helper are prebuilt:

```powershell
$provision = 'C:\motor-spike\hyperv-ally'
powershell.exe -NoProfile -ExecutionPolicy Bypass -File script\hyperv-spike\provision.ps1 `
  -Root $provision -ServerIso C:\Transfer\server2022.iso `
  -GuestAgentPath "$src\bin\motor-winagent.exe" -PeerRunnerPath "$src\bin\motor-runner.exe"
if ($LASTEXITCODE -ne 0) { throw 'Provisioning failed; preserve provision-host.log and guest-logs' }
Get-Content "$provision\sealed.json"
Get-FileHash "$provision\parent.vhdx" -Algorithm SHA256
```

The Hyper-V guest uses `script/windows-vm/setup-job-baseline.ps1`, the same
machine-wide toolchain installer as the QEMU job image. It includes clippy,
rustfmt and cargo-nextest 0.9.138. Nextest's official x86_64 Windows archive is
downloaded and SHA-256-verified during image provisioning, then installed beside
Cargo at `C:\rust\cargo\bin` for offline jobs. Updating these tools requires a
fresh parent image, not a host-side Cargo installation.

Supply the exact public key line from the vault in a file; never transfer the
private key. A registration with `MotorOwner=motor-hyperv-spike` can be
explicitly adopted with `-AdoptSpikeRegistration`:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File script\windows-runner\install.ps1 `
  -Bundle $src -ParentPath "$provision\parent.vhdx" `
  -VaultPublicKeyFile C:\Transfer\motor-vault.pub -LicenseAcknowledged -AdoptSpikeRegistration -AdoptProvisioningRoot
if ($LASTEXITCODE -ne 0) { throw 'Installation failed; preserve the root and installation.json' }
Get-Service MotorRunner
Get-Content C:\ProgramData\ssh\ssh_host_ed25519_key.pub
```

Resolve the host's actual Python executable as the operator, then pass that path
to the SYSTEM wrapper. This needs no SYSTEM rustup/cargo profile:

```powershell
$python = (Get-Command python.exe -ErrorAction Stop).Source
powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\MotorRunner\bin\run-system.ps1 `
  -Python $python -Evidence C:\motor-runner-evidence\single-slot
if ($LASTEXITCODE -ne 0) { throw 'Qualification failed; return the complete evidence directory' }
```

Qualification checks the startup-only parent hash and per-job identity guard; the
SYSTEM local-registry cache and the job VM's offline Cargo fetch/build; a deliberately
partial agent frame and the guest's matching EOF byte count; native loopback and
foreign-VM rejection while the target guest agent's listener is still active (a
connect failure alone cannot pass); five full runner jobs with multi-frame trees,
plus two single-job preparation cases in `qualify-jobs.py`: a bundled Git
dependency pinned by tag built with `cargo build --offline --locked`, and a
repository containing only `run.sh` with no Cargo manifest or lockfile;
cold then warm registry/cache paths, large stdout and source-output collection,
the non-admin `motor-job` guest account, and that host cache bytes do not leak into
the guest's unrelated paths. Every real job first requires `bun --version` to
return exactly `1.4.2` and prints `cargo nextest --version`, requiring
`cargo-nextest 0.9.138`, including jobs using a custom gate command. The configured default commands are `cargo test
--locked --offline -- --nocapture` and `cargo clippy --all-targets --locked --offline --
-D warnings`. It retains host VM-lifecycle samples, startup hash timing, and guest
logs. Any failure stops the service and does not count as a skip. Return the complete
external evidence directory, plus provisioning logs and `sealed.json`, before
enabling routing.

To run just the two preparation cases once each, use a SYSTEM PowerShell session
(not an ordinary administrator shell). Set `$python` in that session to the
absolute interpreter path resolved as the operator above, then run:

```powershell
& $python C:\ProgramData\MotorRunner\bin\qualify-jobs.py `
  --preparation-only --root C:\ProgramData\MotorRunner `
  --evidence C:\motor-runner-evidence\preparation-only
if ($LASTEXITCODE -ne 0) { throw 'Preparation checks failed; preserve their evidence' }
```

The evidence directory must not exist yet, even as an empty directory. This is
a direct SYSTEM invocation; `run-system.ps1` still runs the full qualification.
The default qualification also runs both cases, before the five cache-reuse jobs.

## Diagnose an agent connection failure

Copy `script/windows-runner/diagnose-connect.ps1` to the installed host and run it
from elevated Windows PowerShell 5.1; no new runner binary or source checkout is
needed. Use the diagnostic and control scripts from the same release so their
nonce-media contracts match. It uses the installed `Prepare` operation, probes the agent's hvsock
endpoint for 120 seconds without sending commands, then stops the VM and harvests
the read-only child **before** removing its VM, job directory and private cache.
It refuses to overlap existing runner VM/job resources, and restores a service it
stopped only after cleanup succeeds.

```powershell
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File .\diagnose-connect.ps1 `
  -Evidence ('C:\motor-runner-evidence\connect-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
```

A nonzero result still leaves the protected evidence folder. Return the complete
folder: `EXPECTED-LOGS.txt` explains the attempt trace, passive Hello, agent/build
identity, explicit guest log-path manifest, and agent-filtered System/Application
events (JSON/XML plus exported EVTX). Missing/reparse-refused logs and collection
errors are recorded rather than silently omitted. A socket connection alone is
not protocol qualification; closing the passive probe can produce an expected
guest EOF. No guest passwords, DPAPI files, registry hives or disk images are copied.

## One removal

Before uninstalling, copy any evidence you need to retain from the runner root to a location
outside it. Upgrades use this uninstall/reinstall path; install.ps1 refuses an existing
root, so a fresh sealed parent must be provisioned before reinstalling. The uninstaller
removes the marked SSH block even if an earlier install stopped before recording
`ssh_block`. Copy removal scripts outside the root because the root, sealed parent,
caches and every retained job record will be deleted:

```powershell
New-Item C:\motor-runner-remove -ItemType Directory -Force | Out-Null
Copy-Item C:\ProgramData\MotorRunner\bin\uninstall.ps1,C:\ProgramData\MotorRunner\bin\accounts.ps1 C:\motor-runner-remove
powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\motor-runner-remove\uninstall.ps1 -ConfirmRemoveParentAndState
if ($LASTEXITCODE -ne 0) { throw 'Removal stopped safely; preserve the ownership record and inspect the error' }
```

Removal stops/deletes the service, reconciles/removes owned job VMs and disks, removes only the
owned hvsock key, removes the SSH block/account/key and any legacy motor-prep account, rights,
profile and DPAPI secret, restores saved host settings, and deletes the parent and runner root.
It refuses foreign accounts/VMs/registry keys and reparse roots.

Rebuilding the sealed parent requires the fresh Windows-image provisioning steps above.
`provision.ps1` budgets 1–3 hours for the VM and update phase, with Windows updates dominating;
this is planning guidance, not a measured rebuild time or duration guarantee. Its timer excludes
host-side preparation and final sealing, so record end-to-end duration when upgrading.

The provisioning scratch parent/logs under `C:\motor-spike\hyperv-ally` are also removed when
explicitly adopted with `-AdoptProvisioningRoot` as above. Archive those logs before uninstalling.
Historical or unadopted spike roots are never deleted implicitly. Hyper-V itself is left installed.
