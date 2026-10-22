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
  checks `sshd` syntax and verifies effective public-key-only authentication,
  password, TCP/agent forwarding, PTY and forced-command settings. It also
  configures X11 forwarding off where Windows OpenSSH supports that option. It
  prints sshd's public host key for the client pin. No ambient client SSH configuration is required.
- VM management is a boot-start SCM service running as SYSTEM. Trusted Cargo
  **fetch only** uses a separate batch-only `motor-prep` account, a write-restricted
  token, a kill-on-close/process-limited Job Object and allow-listed staging stdio
  handles only. A one-shot SYSTEM broker creates a private non-interactive window
  station/desktop for the fetch logon session, loads its profile and builds its
  user environment; Cargo itself never runs as SYSTEM or an administrator.
  The fetch view contains copied manifests/lockfiles and empty target
  placeholders, not the job's source files. Runner keys remain under the private
  runner-state ACL. A serialized fetch section revokes each job's grants
  before another prep begins. The runner's Cargo cache is the only persistent
  preparation cache, seeded/published by SYSTEM. The fetch token's distinct logon
  SID grants writes only inside its staging temp tree, not the loaded profile,
  shared host temp or persistent cache. Only selected registry/git results are staged.
  Native prep qualification also executes the pinned `cargo --version` through
  this broker; launch diagnostics name the desktop/profile setup and unsigned
  hex exit status, including `0xC0000142 STATUS_DLL_INIT_FAILED` for loader failure.
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
  Restart to re-verify a changed parent. Startup and jobs trust the record equally.
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

For a pre-main trusted-fetch failure, collect only the launch matrix with
`run-system.ps1 -Python $python -Evidence $freshEvidence -PrepLaunchMatrix`.
This stops MotorRunner and runs sixteen probe/Cargo/rustc token comparisons without
VM qualification. It records requested token flags separately from actual
`IsTokenRestricted`, restricting-SID, integrity and UI-object DACL readbacks.
The Everyone-SID and explicit-desktop-write controls are diagnostic-only and
restricted to pinned `cargo --version`; production continues to use flags `0x9`
and only the logon SID. Additional diagnostic cells compare token-default-DACL
updates and the RESTRICTED SID (`S-1-5-12`); they do not change the production
token. The fixed offline Cargo-fetch cell and follow-up controlled write probes
reuse one created token/profile/desktop per cell. Readbacks retain default DACLs
before/after and actual writes to selected staging/outside/profile/cache paths.
Matrix completion is evidence collection, not a pass or
permission to enable fleet routing. Leave the service stopped for evidence review.
This diagnostic lane temporarily ORs `FLG_SHOW_LDR_SNAPS` (`0x2`) into the
64-bit Image File Execution Options (IFEO) `GlobalFlag` values for `cargo.exe`
and `rustc.exe`, and restores the
exact original value kinds/contents (or absence) in `finally`. It records the
snapshot and verified restoration in `loader-snaps-registry.json`; do not kill
the wrapper while it is restoring these settings. The release includes its own
native debug-event collector, so no gflags, WinDbg or new host installation is
required. Protected `*.loader.jsonl` evidence contains Windows DLL-load events,
loader debug strings and exceptions. Process/thread security-descriptor readbacks
and actual Cargo/Rustup-home values accompany each diagnostic launch. Cells 12–13
isolate runner-before-Cargo and repeated-Cargo ordering on one token.
Cell 14 compares the traverse-only root cwd with the already-authorized staging
temp cwd on the same token/desktop, then calls both RNG probes on that session.
Cells 15–16 independently call dynamically resolved `ProcessPrng` (BOOL) and
`BCryptGenRandom` (NTSTATUS, system-preferred RNG) in a restricted probe with safe
cwd; random bytes are never logged. These cells additionally trace selected x64
ntdll object API entries/returns with decoded object names when available, target
metadata, requested access and NTSTATUS. Breakpoint bytes are patched only in the
disposable debuggee's private memory; one-instruction stepping briefly changes its
thread context and pauses other debuggee threads. Neither affects host DLL files
or production launches. An interrupted step/unwound call aborts collection with
an explicit instrumentation error instead of assigning a later return to it.
Production launches do not enable loader snaps or debugger creation flags.

For the AppContainer network failure, collect the lowbox-only diagnostic with
`run-system.ps1 -PrepLaunchMatrix -LowboxLaunch`. It records no-launch prechecks
for the session's `AppContainerNamedObjects` directory, the `motor-prep` profile
hive/mapping, and the batch-token session. SYSTEM resolves an index.crates.io
address once so token name resolution can be distinguished from raw TCP/Windows
Filtering Platform (WFP) access.
It then compares two variants that create the AppContainer profile under
`motor-prep` impersonation with its hive loaded, but launch through the same
self-created object directory and `NtCreateLowBoxToken` path. The variants are
Less Privileged AppContainer (LPAC) with `internetClient`, `registryRead` and
`lpacCryptoServices`, and plain AppContainer with `internetClient`. This combines profile registration/storage with
the self-created-token launch method. The filesystem-write checks also attempt
writes to the AppContainer profile folder and registry storage. See [Microsoft's AppContainer profile
guidance](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer),
[`NtCreateLowBoxToken`](https://learn.microsoft.com/en-us/windows/win32/secauthz/ntcreatelowboxtoken),
[WinHTTP SSL](https://learn.microsoft.com/en-us/windows/win32/winhttp/ssl-in-winhttp),
and [DNS client RPC behavior](https://projectzero.google/2021/08/understanding-network-access-windows-app.html).

Each token runs the same filesystem-write checks and calls in-token `getaddrinfo`
to record whether name resolution succeeds; this does not test direct reachability
of a DNS server. Raw TCP is separately attempted against the SYSTEM-resolved
index.crates.io address, and WinHTTP tests HTTPS using Schannel TLS. Microsoft's
[WinHttpOpen documentation](https://learn.microsoft.com/en-us/windows/win32/api/winhttp/nf-winhttp-winhttpopen)
lists `ERROR_NOT_ENOUGH_MEMORY` as a generic allocation failure; Win32 14 is
`ERROR_OUTOFMEMORY` (not enough storage), with no documented AppContainer-specific
meaning. It fails before a request or Schannel handshake, so the probe records the
startup error without treating it as a network result. See [system error codes](https://learn.microsoft.com/en-us/windows/win32/debug/system-error-codes--0-499-).
`LsaConnectUntrusted` is reported separately; it does not gate the transport
result or Cargo fetch. If a probe child starts, Cargo fetch uses the first token
passing DNS/TCP/Schannel; if none passes all three, it uses the started token with
the most transport progress. If no probe child starts, Cargo fetch is skipped.
Each report records all token capabilities, Local Security Authority (LSA) status,
network stages, failing API/Win32 error, write attempts and cleanup paths. Per-cell
cleanup records profile deletion, profile-folder/registry state, user-hive unload,
token closure and object-directory disappearance; a final precheck reports leftovers.
The diagnostic attempts to revert persistent grants and records whether their
before/after discretionary access control lists (DACLs) match. It also attempts
to remove staging; an unreaped child defers that cleanup, and the report records
any leftover path. This remains diagnostic evidence, not qualification or a
production policy change.

Run the operator-only lowbox lane from an elevated PowerShell 5.1 prompt after
installing a bundle built from the reviewed source commit:

```powershell
$python = (Get-Command python.exe -ErrorAction Stop).Source
$evidence = 'C:\motor-runner-evidence\lowbox-' + (Get-Date -Format 'yyyyMMdd-HHmmss')
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\MotorRunner\bin\run-system.ps1 `
  -Python $python -Evidence $evidence -PrepLaunchMatrix -LowboxLaunch
if ($LASTEXITCODE -ne 0) { throw "Lowbox diagnostic failed; preserve $evidence" }
```

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
`run-system.ps1` is for qualification only, not installation.

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

Qualification checks the real restricted batch token's denied credential-file
open and write outside staging/the runner's Cargo cache, with an unrestricted
batch-token outside-write control
and a successful restricted staging write; a deliberately partial
agent frame and the guest's matching EOF byte count; native loopback and foreign
VM rejection while the target guest agent's listener is still active (a connect
failure alone cannot pass); five full runner jobs with multi-frame trees, cold then warm real cache,
large stdout and source-output collection, the non-admin `motor-job` guest
account, and a check
that a canary in the host prep cache is absent from the guest's Cargo home. The
configured default commands are `cargo test --locked --offline -- --nocapture`
and `cargo clippy --all-targets --locked --offline -- -D warnings`. It retains
host VM-lifecycle samples, parent hashes and guest logs. Any failure stops the
service and does not count as a skip. Return the complete external evidence
directory, plus provisioning logs and `sealed.json`, before enabling routing.

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

Copy evidence out first. Copy removal scripts outside the root because the root,
sealed parent, caches and every retained job record will be deleted:

```powershell
New-Item C:\motor-runner-remove -ItemType Directory -Force | Out-Null
Copy-Item C:\ProgramData\MotorRunner\bin\uninstall.ps1,C:\ProgramData\MotorRunner\bin\accounts.ps1 C:\motor-runner-remove
powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\motor-runner-remove\uninstall.ps1 -ConfirmRemoveParentAndState
if ($LASTEXITCODE -ne 0) { throw 'Removal stopped safely; preserve the ownership record and inspect the error' }
```

Removal stops/deletes the service, reconciles/removes owned job VMs and disks,
removes only the owned hvsock key, removes the SSH block/account/key and prep
account/rights, restores saved host settings, and deletes the parent and runner
root. It refuses foreign accounts/VMs/registry keys and reparse roots. The
provisioning scratch parent/logs under `C:\motor-spike\hyperv-ally` are also
removed when explicitly adopted with `-AdoptProvisioningRoot` as above. Archive
those logs before uninstalling. Historical or unadopted spike roots are never
deleted implicitly. Hyper-V itself is left installed.
