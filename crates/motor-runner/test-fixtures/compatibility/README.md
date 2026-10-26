# Identity compatibility baselines

These outputs were captured on Linux from the production code at
`f15a58364ee42fb44d7bd06d5f3ea8ffe6f9aee9`. At that revision an identity path
means the host checkout and the caller-visible checkout use the same absolute
path. The baselines precede support for mapping a caller prefix to a different
host directory and for distinguishing host and caller paths with Rust types.
Only test code was added during capture. The tests compare
against committed bytes, not a second serialization of an expected value.
There is no update-on-test mode. Do not regenerate these fixtures just to make
tests pass; a deliberate compatibility change must explain each changed
baseline. Adding path mapping must preserve the identity-path baselines.

Run the focused tests with the repository's Git-configuration cleanup:

```sh
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
  cargo test -p motor-runner --lib compatibility
```

## Coverage and normalization

- `users-sandbox-argv.txt`: the complete durable `Sandbox::command` argv with
  literal `/Users/u/repo` binds and `/Users/u/repo/src` cwd. It uses the existing
  command-only sandbox fixture, so it does not create or mount `/Users` on the
  test host. Only the scratch root and random unit id are replaced. The
  existing multi-profile argv SHA-256 goldens remain unchanged.
- `users-sibling-binds.txt`: production `sibling_mounts`, `safe_directories` and
  `write_safe_directories` output for literal `/Users` paths, in both in-place
  and snapshot modes, including the alias. No path normalization is applied.
- `identity-bind-list.txt`: the complete ordered read-only/read-write mount
  vectors and `.gitconfig` from real `Sandbox::prepare_role` planning. This
  fixture uses a physical scratch checkout, not a literal `/Users` checkout;
  privileged account/permission commands are stubbed by the existing process
  hook, as is dependency fetching against synthetic toolchains. It includes a
  Cargo target, `.git`, a sibling and its alias. The host-discovery fixture
  supplies the environment values and filesystem answers at the same reads
  used by production planning: `CARGO_HOME=/motor-home/cargo`, a fixed Rustup
  root, Bun present at `/home/ubuntu/.bun/bin`, both Cargo `registry` and `git`
  directories present, and all eleven system paths in the original capture
  present. Unlisted host probes fail instead of consulting the machine.
  Overrides are thread-local and reset on unwind; they do not mutate the
  process environment or create host-global directories. Scratch, sandbox
  runtime, the pinned Cargo root and the pinned Rustup root are replaced by
  their named placeholders, retaining every suffix and source/destination
  pair. In particular the original Cargo root equals the private job Cargo
  root, so both ends of its two binds carry the same placeholder. No mount
  entries are added, filtered or sorted after planning. The committed bytes
  remain those captured from the original production revision.
- `hello.json` and `hello-snapshot.json`: server Hello payloads from real Unix
  socket sessions, with all client capabilities offered, in legacy and sibling
  snapshot modes. The existing portable snapshot configuration avoids needing
  a running systemd manager. A fixed toolchain version supplied through the VM
  config field avoids the startup `rustc --version` probe, while the backend
  remains Host; the unused VM image/helper paths are never opened. Compiler
  versions are not part of Hello. JSON bytes have no trailing newline.
- `catalog.json`: bytes written by `atomic_json` for a synthetic identity
  catalog containing a workspace, a repository's desired base, a legacy
  checkout, sibling commits, a UUID-keyed retained transfer, and global fields.
- `job-record.json`: bytes written by `atomic_job_json` for a synthetic terminal
  `/Users` job, including its request, Started progress and terminal record.
  Times, ids and commit/tree hashes are fixed inputs. Both persistence tests
  also deserialize and reserialize the recorded bytes. JSON bytes have no
  trailing newline, and neither persistence fixture has path normalization.
- `return-result.json`: production non-chunked collection of a one-byte file.
  The physical scratch workspace root is replaced with `/Users/u/repo` only
  in the serialized result. The request uses an absolute return path, as the
  runner currently requires. The golden pins the returned
  name, digest and base64 bytes; it does not assert reads of the real `/Users`.

The companion tests also pin Unix identity spelling validation (including
interior `.`, repeated separators and trailing slash) and the exact macOS
`/home` refusal detail. They do not introduce caller-root configuration or
change the existing Windows mapping tests or protocol vectors. When prefix
mapping is supported, identity jobs with mappings configured should be
compared to these same baselines, not new copies.

## Host-input audit

The argv fixture supplies its own toolchain paths, user and group names and
bind vectors through the command-only sandbox fixture; it does not discover
host tools. Sibling binds use literal paths and do not probe their existence.
The prepared-bind fixture pins every external environment and existence input
as described above; its workspace and sibling Git trees are created under
scratch. Account lookups are stubbed, while job usernames remain production
constants (`motor-job` and `motor-cache`). Hello is generated by explicit
portable runner configurations. Persistence records use fixed literal data,
and returned-file collection reads a file the fixture itself creates. None of
the other goldens consults Bun, a Cargo/Rustup installation or optional system
files to choose its output. All compatibility tests must pass with arbitrary
outer `HOME`, `CARGO_HOME` and `RUSTUP_HOME` and without Bun on `PATH`, as well
as in the ordinary build environment, without changing any golden bytes.
