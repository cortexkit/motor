# Third-party notices

Motor is licensed under the MIT license in [LICENSE](LICENSE). This file lists
material in the repository that was adapted or copied from other projects, with
each source's license and the files it covers. Dependencies fetched from
crates.io at build time are not listed; their licenses ship in their own
packages, at the versions in `Cargo.lock`.

## Elysia (Bun install measurement fixture)

- Source: elysiajs/elysia 1.3.21, commit
  `dafd31b516089e1f6844f98dddaceefd0436da23`, <https://github.com/elysiajs/elysia>
- License: MIT, Copyright 2022 saltyAom
- Files, copied verbatim:
  - `script/runner/network-fixture/package.json`
  - `script/runner/network-fixture/bun.lock`
  - `script/runner/network-fixture/LICENSE` (the upstream license text)

These files are install inputs for a network measurement, not source code.
`script/runner/network-fixture/README.md` records their SHA-256 digests.

## Firecracker API specification (tenant-VM spike fixture)

- Source: Firecracker v1.17.0 release, `firecracker_spec-v1.17.0.yaml` from
  `firecracker-v1.17.0-x86_64.tgz`,
  <https://github.com/firecracker-microvm/firecracker/releases/tag/v1.17.0>
- License: Apache License 2.0, Copyright Amazon.com, Inc. or its affiliates
- Files:
  - `script/tenant-vm-spike/fixtures/firecracker_spec-v1.17.0.yaml` (verbatim)
  - `script/tenant-vm-spike/fixtures/firecracker_spec-v1.17.0.json` (a
    generated JSON projection of the YAML)
  - `script/tenant-vm-spike/fixtures/LICENSE-APACHE-2.0` (the license text)

`script/tenant-vm-spike/fixtures/README.md` records the YAML's SHA-256 and how
the projection is regenerated and checked.

## AFT Windows VM setup script

- Source: AFT, <https://github.com/cortexkit/aft>, `scripts/windows-vm/setup-guest.ps1`
- License: MIT
- Files: `script/windows-vm/setup-guest.ps1`, adapted for a Rust and Bun build
  image.

## Prefrontal build-slot supervisor

- Source: the CortexKit prefrontal repository, revision `d529e17eb`
- License: covered by this repository's MIT license, as recorded in
  [`crates/prefrontal-build-slot/VENDORED.md`](crates/prefrontal-build-slot/VENDORED.md)
- Files: `crates/prefrontal-build-slot/Cargo.toml`,
  `crates/prefrontal-build-slot/rustc-lowprio-direct.sh`,
  `crates/prefrontal-build-slot/src/main.rs`,
  `crates/prefrontal-build-slot/tests/it/lifecycle.py`,
  `crates/prefrontal-build-slot/tests/it/main.rs`,
  `crates/prefrontal-build-slot/tests/it/wrapper.rs`, and
  `scripts/tests/test_build_budget.py`. `VENDORED.md` lists their digests.

## cortexkit-exec-remote-types test vectors

- Source: the published `cortexkit-exec-remote-types` 0.2.0 crate,
  <https://crates.io/crates/cortexkit-exec-remote-types>
- License: MIT
- Files: the `crate-local-unknown-*.jcs` and `crate-local-unknown-*.sha256`
  files under `test-vectors/exec-remote-v1/outcomes/` and
  `test-vectors/exec-remote-v1/replies/`, copied byte for byte.
