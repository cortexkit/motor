# Motor worker rules

## Build and verify

- Build and test on the build server: pass `runon: "linux,4c"` to the worker's
  `bash` tool, replacing `4` with the vCPU count in the brief. Never build on the
  local Mac, except native macOS clippy and macOS unit tests that start no VM.
- Before delivery run `scripts/gate.sh --fast` (format, pins, safety checks),
  clippy with `-D warnings` for the crates you touched, and only the tests for
  what you changed: select them by the test modules that cover the changed
  files (for example `-E 'test(/^tests::broker::/)'`), not by a keyword that
  may miss them. Never run a whole crate or workspace suite or the full gate;
  the parent runs the full suites once over several merged deliveries.
- A failure that looks unrelated: rerun that one test by name on the base
  commit, never the suite.
- A filtered run that takes longer than about 15 minutes means the filter is
  too broad. Don't wait on it in 30-minute `bash_watch` loops, and keep the
  normal build directory (a fresh `CARGO_TARGET_DIR` forces a cold build).
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
