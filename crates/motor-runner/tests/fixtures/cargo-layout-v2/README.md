# Captured Cargo build-directory layouts

These trimmed trees were captured on Linux with Cargo 1.100.0
(`3acc79c0c`, rustc `0ef1bc386`) and Cargo 1.99.1 (rustc `58cf8f9c3`).
The registry-free workspace has an app library with a unit test, a path library
dependency with a build script, and a path proc macro. The capture ran
`cargo +<version> test --workspace --no-run --offline` with `CARGO_TARGET_DIR`
set to `<ROOT>/target` and `CARGO_BUILD_BUILD_DIR` set to
`<ROOT>/target/bd/ab/0123456789abcd`.

Entries are names only, except the indented first lines of rustc dep-info files.
`<ROOT>` replaces the job's temporary directory. The trees omit repeated test
units, JSON fingerprints and incremental session contents. The hashes and
remaining names are unmodified. V2 has no `deps/` or `.fingerprint/`; outputs,
fingerprints and build-script execution state share `build/<package>/<hash>/`.
Test executable filenames still include hashes in this capture. Incremental
sessions remain under `debug/incremental/` in both versions (omitted here).

`fixture.rs` materializes the names and dep-info headers for ownership and
stale-unit tests. Non-dep-info file contents are deliberately synthetic.
