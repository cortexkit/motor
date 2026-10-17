# Firecracker API contract fixture

`firecracker_spec-v1.17.0.yaml` is an unmodified artifact from the official
`firecracker-v1.17.0-x86_64.tgz` release. The operator verified the tarball
against its published `.sha256.txt` and the extracted schema against the
release's `SHA256SUMS` entry for `./firecracker_spec-v1.17.0.yaml`.

Source: <https://github.com/firecracker-microvm/firecracker/releases/tag/v1.17.0>.
YAML SHA-256: `473a0a392927b407834b341bf5fbc7925524fedaf9d925e1bbbafb6a1f48c671`.

The schema is **Apache-2.0**, not the surrounding repository's MIT license.
The upstream license is retained here as `LICENSE-APACHE-2.0`.

The JSON file is a complete generated projection of this YAML, not a second
hand-maintained API model. It lets the host probe and portable tests validate
requests using only Python's standard library. Both fixture digests are pinned
in `api_schema.py`. To regenerate or verify the projection with PyYAML installed:

```sh
python3 script/tenant-vm-spike/generate-api-spec.py
python3 script/tenant-vm-spike/generate-api-spec.py --check
```

The validator follows the published request schemas (refs, required fields,
types, enums, numeric bounds and nested arrays/objects), additionally closing
request objects to undeclared keys as the Firecracker API does. Swagger 2.0
does not itself mark these objects `additionalProperties: false`. Snapshot-load
memory-source exclusivity is a prose constraint in the published schema and
is checked explicitly. This is not a claim to implement all of OpenAPI or
all Firecracker runtime checks.
