# Draining, deploying and rolling back the runner

A runner restart is not a completion barrier: a job that is still being set up
when the daemon stops cannot be resumed. Drain the runner first, then swap the
binary. `script/runner/deploy.sh` does both and rolls back on failure. This
guide explains the drain it relies on and how to reverse a deploy.

Install the runner first; see [runner-install.md](runner-install.md).

## How drain works

Drain closes admission and waits for accepted work to reach a state that
survives a restart.

- New `run` and `prepare` requests are refused before start with
  `runner_draining` and `ran: none`. The refusal carries `retry_after_ms`, the
  time left until the drain deadline. Test:
  `drain_retry_hint_is_bounded_by_the_deadline_on_the_private_frame`
  (`crates/motor-runner/src/tests.rs`).
- Jobs that are queued but not started are also refused with `runner_draining`;
  their commands never start. Test:
  `drain_refuses_new_and_queued_jobs_without_starting_queued_commands`
  (`crates/motor-runner/src/tests.rs`).
- Drain waits for jobs that are being admitted, reserved or set up, because
  setup cannot resume after a restart. Test: `drain_waits_for_a_job_still_preparing`.
- A started Linux job that runs in a recorded systemd unit does not hold the
  drain: it keeps running, and the restarted daemon reattaches it. The drain
  reply counts these jobs in `restart_recoverable_jobs`. Test:
  `drain_completes_promptly_with_a_started_unit_backed_job` (Linux only).
- A started Windows job cannot reattach, so it holds the drain until it ends.
  Test: `windows_started_unit_backed_job_keeps_drain_waiting`
  (`crates/motor-runner/src/tests/windows_instance.rs`).
- Concurrent drain requests share one deadline. A later request can shorten it,
  never extend it, and the first request decides whether the daemon exits.
  Tests: `concurrent_drains_answer_at_the_earliest_requested_deadline`,
  `shorter_first_drain_still_answers_later_longer_request`.
- When the deadline passes first, the reply has `timed_out: true`. That does not
  mean jobs finished.

The default deadline is 1800 seconds, capped at 86400. Three ways to start a
drain:

```sh
# Over the control socket; prints the result and exits nonzero on timeout.
motor-runner drain --instance <instance> --drain-timeout-s <seconds> --exit-after-drain false
# By signal; the daemon uses the deadline from its own --drain-timeout-s and exits afterwards.
systemctl --user kill --kill-whom=main --signal=SIGUSR1 motor-runner@<instance>.service
# Reopen admission after a drain that held the daemon.
motor-runner resume --instance <instance>
```

Pass `--state-root <path>` to `drain` and `resume` when the instance does not
use the default state root (`$XDG_DATA_HOME/cortexkit/motor-runner`, or
`~/.local/share/cortexkit/motor-runner`). The instance named `main` runs as
`motor-runner.service`; other instances run as `motor-runner@<instance>.service`.

With `--exit-after-drain false` the daemon stays up with admission closed until
it is stopped or resumed. `deploy.sh` uses this so that systemd's restart policy
cannot admit jobs between the drain and the swap. Test:
`resume_reopens_admission_on_the_owner_admin_path`.

## What a restart does to unfinished jobs

At startup, before accepting connections, the runner reads its durable job
records.

- A job that was waiting for bundles or queued, with no launch identity and no
  start time, ends as `refused_before_start` with reason `lost_to_restart` and
  `ran: none`, under its original job ID. Test:
  `queued_job_restart_returns_lost_to_restart_promptly_without_resubmission`
  (`crates/motor-module/tests/it/recovery.rs`).
- A job with a recorded launch identity or start time never receives a
  before-start refusal, because its command may have run.
- A surviving Linux unit is reattached with its lease, cancellation and timeout,
  and its retained output replays. Test:
  `isolated_restart_reattaches_surviving_unit_and_replays_output` (operator-only).
- A job whose launch was recorded but whose exit code was not ends as
  `outcome_unknown`. Test:
  `crash_before_gate_byte_runs_nothing_and_startup_loses_the_record`
  (`crates/motor-runner/src/tests.rs`, Linux only).

## Deploy a new runner binary

On the build server, as the account that owns the runner's user units:

```sh
script/runner/deploy.sh /absolute/path/to/new/motor-runner [instance]
```

The instance defaults to `main`. The script:

1. Runs the candidate's `--version` and stages it next to the live binary as
   `<live>.new`, without touching the running executable.
2. Asks the runner to drain with `--exit-after-drain false`. The deadline is
   `RUNNER_DRAIN_TIMEOUT_S`, default 600 seconds.
3. If the drain times out, runs `script/runner/drain_survivors.py` over the
   instance's job records. It proceeds only if every unfinished job is safe to
   restart: proven not started, backed by a recorded Linux unit, or an exited
   process. A started Windows job, an unreadable record, or an uncertain job
   aborts the deploy.
4. Copies the live binary to `<live>.rollback-<UTC timestamp>`, stops the unit,
   moves the candidate into place, starts the unit, and checks it is active.
5. Prints `deployed <unit>; rollback executable: <path>`.

On any failure before the stop, the script reopens admission on the old daemon
with `resume`; it never signals the daemon. On a failure after the stop, it
copies the rollback binary back and starts the unit. If either recovery fails,
it prints the manual step.

Environment overrides: `RUNNER_LIVE_BINARY` (the live executable path),
`RUNNER_STATE_ROOT`, `RUNNER_DRAIN_TIMEOUT_S`. The `windows` instance uses its
own state root by default; see `deploy.sh`.

Tests: `python3 -m unittest -v script/runner/test_deploy.py`, including
`test_failed_swap_start_rolls_back_and_reopens`,
`test_timeout_unsafe_survivor_reopens_admission`,
`test_timeout_started_windows_unit_reopens_admission` and
`test_unacknowledged_drain_reopens_admission`.

## Verify

```sh
systemctl --user is-active motor-runner.service     # or motor-runner@<instance>.service
journalctl --user -u motor-runner.service -n 50     # look for "motor-runner serving" and its version
<live binary> --version
```

## Roll back

`deploy.sh` keeps every replaced binary as `<live>.rollback-<timestamp>`. To
return to one, deploy it the same way, so the rollback also drains first:

```sh
script/runner/deploy.sh <live>.rollback-<timestamp> [instance]
```

A rollback swaps the binary only; it does not restore the runner's state on
disk. The newer binary may have migrated that state at startup
(`crates/motor-runner/src/migration.rs`). `scripts/mixed-version.py` proves
wire compatibility between versions; that an older runner reads state written
by a newer one is not covered by a test.


## SUBC module rollback is a separate storage handoff

The runner instructions above do not export ck-motor's device-local store.
Before SUBC replaces ck-motor with an older binary, stop it with SIGTERM and wait
for successful checkpoint export and exit. After a crash or failed shutdown
export, stop the daemon and run the new binary's
`ck-motor store-checkpoint --root /path/to/store.db.ck-motor`; require exit status
0 before replacing the binary. Never delete its journal to make an old binary
start. An unexported rollback can lose cancellation/Run fences since the last
checkpoint. See [the complete ck-motor rollback procedure](module.md#subc-rollback-procedure)
for the five-minute healthy-checkpoint bound and failure handling. Tests:
`rollback_clean_shutdown_exports_every_transition_for_222d833`,
`rollback_offline_checkpoint_exports_crash_suffix_for_222d833`.
