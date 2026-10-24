# Run the Cargo preflight only for a tree that builds with Cargo.
#
# The writer shell calls this before the fill command. A repository can also
# declare a fill for a tree with no root Cargo.toml (a C project, say), and Cargo
# has nothing to say about that tree: `cargo metadata` would fail and the fill
# would be skipped. The manifest check is done here, where the file actually is.
if test -f Cargo.toml; then
    exec python3 /motor-tools/writer-preflight.py </dev/null
fi
