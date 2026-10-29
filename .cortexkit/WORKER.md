# Motor worker rules

## Build and verify

- Build and test on the build server: pass `runon: "linux,4c"` to the worker's
  `bash` tool, replacing `4` with the vCPU count in the brief. Never build on the
  local Mac, except native macOS clippy and macOS unit tests that start no VM.
- Run `scripts/gate.sh` before delivery. For a change confined to part of the
  code, run at least `scripts/gate.sh --fast` and the lanes covering the touched
  crates; `scripts/gate.sh --help` lists the lanes. The full gate runs CI's
  ordinary checks, not the privileged operator suite.
- For a bug fix, show once that the new regression test fails without the fix.

## Pre-approved: proceed without asking

- Regenerate pins and goldens when every difference follows from the cause
  described in the brief or your change description.
- Merge main while keeping both sides' changes.
- Add test-only seams that do not stub permission or account checks.
- Make a test load-independent by fixing the cause of its load dependence.
- Add `#[cfg(...)]` gates to fix a platform lint.

## Ask the parent first

The parent is the agent that assigned the work. Call the worker's `ask` tool
with a concrete `question` and enough `context` to decide without opening files.

- Wire formats and contract changes.
- Anything weakening isolation, permissions or the job sandbox.
- Stored data formats and migrations.
- Deleting or loosening an assertion.
- Public documentation wording.
- Anything changing production deployment steps.

## Common traps

- Root-only `isolated_*` tests run only on the build server, as the operator,
  outside remote jobs. Ask the parent for that run; never count a skip as a pass.
- Never stub security checks under `cfg(test)`.
- Motor is public: internal agent names and fleet details never go in `docs/`.
- Put test scratch files under the shared scratch root, never the system temp
  directory. Rust tests use `cortexkit_test_support::scratch_root()`. For scripts
  on Linux use the runner's `TMPDIR` (operator default `/srv/build/tmp`); it must
  support reflinks.
- Tests using `Runner::open` take `manual_tick` when they advance the clock or
  count background work.
