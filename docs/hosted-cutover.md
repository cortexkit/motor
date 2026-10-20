# Hosted runner cutover

The hosted path is an opt-in Linux route. `hosted_runners` defaults to false;
Windows keeps its existing route. Disabling hosted routing sends **new** Linux
jobs through the existing SSH path. Attach, cancel and recovery of existing jobs
use their saved route, even after a config change or module restart.

This document is a procedure, not a production qualification record. A portable
test run does not establish a working tenant VM, a deployed broker or a completed
24-hour observation window.

## Prerequisites

- A registered host, provisioned tenant, working TLS endpoint and production
  broker. [hosted-host.md](hosted-host.md) provisions the host and tenant. The mirrored contract and vectors are under
  `crates/motor-lease/contracts/cloud/`, pinned to the CLOUD revision recorded in
  its manifest. Do not run production against the fake broker.
- An account grant for ck-motor's outbound route to the claustrum vault. The
  daemon authenticates this caller as `reserved:ck-motor` using the supervised
  module's launch nonce; this is not an inbound exec route to ck-motor.
  CKCRED must support `credential.account_token` for audience
  `cortexkit-runners`. A person must run `ck account login` if authentication is
  required; ck-motor does not open an interactive login.
- A rollback binary and saved off-mode configuration, with the ordinary SSH
  credential and runner still usable. Test that binary against the saved config
  before enabling the hosted route.
- A caller that treats a hosted refusal as **no execution**, never permission to
  run locally. The published caller types are pinned at 0.2.8: hosted safety and
  billing fields remain in ck-motor job state rather than new external fields.
  Future external typed flags and their adoption by the agent's execution tool
  and scheduler remain separate caller-side prerequisites, not claims made by
  this cutover procedure.

The default config omits `hosted_runners`, `broker_url` and `broker_token_source`
so it loads in the pinned older client. Do not leave explicit false/default
hosted keys in the config used by a binary that rejects unknown fields.

## Refusal mapping at the 0.2.8 boundary

Before-start refusals have typed `Outcome::RefusedBeforeStart { reason }` and
`ran: none`. Their display-only `refusal_detail` starts with the code below.
ck-motor also saves `hosted_no_fallback: true` and `hosted_code` in job state.
These are not fields added to the published 0.2.8 types. No hosted refusal opens
an SSH connection or executes a local command.

| Hosted condition | Existing `RefusalReason` | Detail code |
| --- | --- | --- |
| Broker 401/403, host authentication refusal | `RunnerAuthenticationFailed` | `hosted_auth` |
| Expired lease | `RunnerAuthenticationFailed` | `hosted_expired` |
| TLS SPKI mismatch | `RunnerAuthenticationFailed` | `hosted_spki_mismatch` |
| No account grant/machine token (`not_found`) | `RunnerAuthenticationFailed` | `hosted_account_not_found` |
| Person must run `ck account login` (`auth_required`) | `RunnerAuthenticationFailed` | `hosted_account_auth_required` |
| Account token service temporary failure (`transient`) | `Unreachable` | `hosted_account_transient` |
| Account token service permanent refusal (`permanent`) | `RunnerAuthenticationFailed` | `hosted_account_permanent` |
| Invalid or unusable account token reply | `RunnerAuthenticationFailed` | `hosted_account_invalid` |
| Host full after bounded placements | `RunnerFull` | `hosted_host_full` |
| No broker capacity after bounded placements | `RunnerFull` | `hosted_capacity` |
| Connection failure before Run | `Unreachable` | `hosted_connect_failed` |
| Broker unavailable | `Unreachable` | `hosted_broker_unreachable` |
| Budget exhausted | `RequestRejected` | `hosted_budget_exhausted` |
| Broker platform unavailable | `PlatformUnsupported` | `hosted_platform_unsupported` |
| No timeout or runtime cap below minimum | `RequestRejected` | `hosted_no_timeout`, `hosted_max_s_too_short` |
| Path outside supported roots | `RequestRejected` | `hosted_path_outside_roots` |
| Hosted workspace prepare/drop | `RequestRejected` | `hosted_prepare_unsupported` |
| Return path limits/capability missing | `RequestRejected` | `return_paths_exceeded`, `return_paths_unsupported` |
| Runner lease mismatch | `RequestRejected` | `lease_mismatch` |
| Generic pre-start request rejection | Existing validation reason | `hosted_request_rejected` |

`transient` returns the vault's optional `retry_after_ms` on the terminal. The
caller may retry a refused request after that delay; ck-motor does not retry
non-transient account refusals or silently switch routes. Vault error codes and
messages are not copied into logs or persisted refusal details.

Conditions without an exact 0.2.8 refusal variant, for the caller-types owner:

- Budget exhaustion has no budget-specific reason; it uses `RequestRejected`.
- Account grant/login/permanent failures share `RunnerAuthenticationFailed`.
- The contract has no typed external hosted-no-fallback flag or hosted code.
- A **lost host after Run may have been sent is not a before-start refusal**.
  It is `Outcome::OutcomeUnknown`, not permission to retry the command. If a real
  terminal is already stored, keep that result even when billing is unavailable.
- `hosted_job_admitted` is recovery via attach, not resubmission. Settled/released
  jobs (`hosted_job_finished`, `hosted_job_released`) do not authorize a fresh Run.

Tests: `hosted_config_defaults_omit_all_keys_and_fake_is_forbidden_on_main`,
`hosted_refusals_use_legacy_reason_and_distinct_codes`, and the account-token
fake-vault tests in `crates/motor-module/src/hosted/account/tests.rs` and
`crates/motor-module/src/hosted/tests/account.rs` cover the local contract. These
substitute the vault transport; they do not qualify a live daemon, launch nonce
or CKCRED service. A separate in-process fake daemon test,
`account_vault_transport_uses_authenticated_claustrum_route_and_exact_envelope`,
checks the production SDK transport's route identity and wire envelope without
claiming real service or nonce-issuance qualification.

## Enable on one machine

1. Preserve the current unit, config and rollback binary. Record the **deployed**
   runner unit's `--max-weight` and `--windows-cpus`; repository defaults are not
   evidence of the deployed values.
2. Record the tenant's registry and runner admin response before the flip:
   - `vcpu_ceiling` equals that deployed `--max-weight`;
   - `cpuset` excludes the deployed Windows CPU set;
   - `guest_ram_mib` is at least `memory_max(vcpu_ceiling)` in MiB;
   - `disk_quota_bytes` is sized for that host: the tenant disk, a suspend
     snapshot and its size base must leave free space at or above the refusal
     floor of every runner sharing the filesystem. `provision.py` refuses a disk
     that would not ([hosted-host.md](hosted-host.md#sizes-beside-the-runner)).
     A tenant on the runner's filesystem makes that runner evict idle
     workspaces sooner; a dedicated disk for tenant storage avoids that;
   - `args_status` reports `sccache_size_gib` of a quarter of the disk in GiB
     (at most 200), plus the expected owned roots, free-space floor and maximum
     weight from `/tenant/runner-args`.
   `memory_max(vcpus)` is `max(16 GiB, 3.5 GiB × vcpus)`. Do not substitute the
   smaller test-tenant values for production provisioning.
3. Keep the host agent and tenant VM beside the existing SSH runner. Move caches
   onto the persistent tenant disk; allow for a one-time rebuild. Complete the
   operator-only isolation and recovery qualification before taking real jobs.
4. In that machine's existing ck-motor config, set `hosted_runners: true` and
   `broker_url` to the production HTTPS broker origin. Leave
   `broker_token_source` absent (the default is `ck_account`). Reload/restart the
   module using the normal deployment process. Never put the JWT in the config.
5. Run a small Linux job. Check the saved hosted route, placement, runtime cap,
   real command result and billing state. Confirm an account refusal returns a
   before-start terminal without SSH or local execution. Confirm Windows still
   uses its ordinary runner.

For each broker request, ck-motor asks the vault for a token with sufficient TTL
for that request. Only the vault caches/re-mints tokens. ck-motor must not store
or log the bearer, lease token or challenge signature.

## Observe for 24 hours

Keep the switch on continuously for at least 24 hours. Record UTC start/end and
module/host revisions. Reconcile jobs by durable job ID, not the number of
connections or attach attempts. Store the production record outside this public
repository; do not commit host names, account identifiers, tokens or logs here.

Record at least:

- Hosted jobs submitted and Runs acknowledged, including separately named drill
  jobs; distinguish pre-start refusals from acknowledged Runs.
- Terminal records actually delivered to callers (not just queued), deduplicated
  by job ID; include real terminals and `outcome_unknown` deliveries separately.
- Every `outcome_unknown` job ID, its cause, and any named drill responsible.
- Lost results: each acknowledged Run whose caller received neither its terminal
  nor `outcome_unknown`. This count must be zero.
- Outside named drills, `outcome_unknown` must be zero. Investigate any exception
  before accepting the cutover. Billing pending/unavailable is not itself a lost
  result when the real terminal was delivered.
- Broker authentication/capacity errors, usage outbox backlog/divergence, tenant
  restarts, attach recovery, quota/space pressure and account-token refusals.

The operator owns the production flip, the 24-hour counts and their causes.
The caller execution-tool owner owns support for future typed hosted flags; the
scheduler owner owns job attribution. Neither is certified by the portable tests
in this repository.

## Roll back

1. Set `hosted_runners` off for new jobs while leaving the current hosted-capable
   binary running. Submit a new Linux job and record that it takes SSH.
2. Keep at least one in-flight hosted drill job through this flip. Attach/cancel
   on its saved hosted route; let it finish and record delivery of its terminal.
   Do not move or resubmit it on SSH.
3. Wait until **no hosted job is non-terminal**. Check saved job state, not just
   the set of live client connections; detached jobs still count. Preserve job
   history and the tenant disk.
4. Remove **all three** keys from the rollback config: `hosted_runners`,
   `broker_url`, `broker_token_source`. A false value is not removal. Start the
   rollback binary against that exact config and record its revision, successful
   startup and a new SSH job's terminal delivery.
5. Preserve the observation and drill records, including a zero lost-results
   reconciliation. A failed rollback startup or any missing result is a failed
   drill, not a completed cutover.

The stored-route invariant is exercised by
`hosted_route_survives_switch_flip_restart_cancel_status_and_attach`; the Linux
`scripts/mixed-version.py` gate checks the pinned older binary with the default
hosted keys omitted. The live rollback-binary startup and 24-hour record still
require the operator's dedicated environment.
