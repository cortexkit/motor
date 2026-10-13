# Vendored build-slot supervisor

Prefrontal source revision: `d529e17eb` (revision supplied with the extraction).
The imported files are preserved byte-for-byte. The revision belongs to the
originating prefrontal history, not this filtered motor history.

The vendored files are covered by the repository's root [MIT license](../../LICENSE).

The runner embeds this dependency-free supervisor's source at compile time and
compiles it for trusted warm builds. Keeping the source at
`crates/prefrontal-build-slot/` avoids fetching registry source at daemon runtime
or compiling an uploaded supervisor with daemon privileges. Prefrontal retains
its own copy; both copies need synchronized security fixes until a shared
source-distribution API replaces vendoring.

SHA-256 digests of the imported files (paths relative to this directory):

| File | SHA-256 |
| --- | --- |
| `Cargo.toml` | `df38a279274641faf126b901ee909fa49cf03ebe73673ff072ee88934b9e78c3` |
| `rustc-lowprio-direct.sh` | `9e6dd5eaf64e1bcae2c82a95d37f53d06c1f8215c9981b32497796a887160ec5` |
| `src/main.rs` | `ff372df7a71fff37cf28b0a2b109f3c8d4a17f86fb36f60d817feb38d3c5c718` |
| `tests/it/lifecycle.py` | `a739a77e92a7f2409e320339e500a97d753efa635d4ce780b56c3aaf67805c75` |
| `tests/it/main.rs` | `ade87a9b10a94604d1a619f4b3648cb1ec4978a2ed8a96dc4996006c1f7e88b8` |
| `tests/it/wrapper.rs` | `be3f0086c067a044ba811954ce3597ed0d539554b094a86ea3b674024c4ac4eb` |
| `../../scripts/tests/test_build_budget.py` | `577f98512fa637b0c75cac555bfc5da4e5169185c44587e31e8227fff37f0d6b` |

The filtered import of motor's history from prefrontal omitted
`scripts/tests/test_build_budget.py`, the standard-library-only base fixture
required to run the supervisor's lifecycle tests. It was restored unchanged
from prefrontal's frozen source revision, `d529e17eb`.

This receipt excludes itself. A cross-repository parity gate is a separate
follow-up; these digests record the imported bytes, not an independent check of
the source revision.
