# Public Bun install measurement fixture

Inputs are copied verbatim from **elysiajs/elysia 1.3.21**, commit
`dafd31b516089e1f6844f98dddaceefd0436da23`:

- `https://raw.githubusercontent.com/elysiajs/elysia/dafd31b516089e1f6844f98dddaceefd0436da23/package.json`
- `https://raw.githubusercontent.com/elysiajs/elysia/dafd31b516089e1f6844f98dddaceefd0436da23/bun.lock`
- `https://raw.githubusercontent.com/elysiajs/elysia/dafd31b516089e1f6844f98dddaceefd0436da23/LICENSE`

The lock contains **413 resolved package entries**, including optional platform
variants. The installed count depends on the platform; the measurement records
both the lock inventory and Bun's raw output. All manifest dependencies use the
public npm registry; there are no local workspaces or source checkout requirements
for installation. The upstream MIT license is retained in `LICENSE`.

The fixture is only install input, not a source checkout. Do not run its upstream
build/test/release scripts. Install with `--ignore-scripts --frozen-lockfile`.
The operator job copies the two inputs from non-manifest filenames only after
its gate opens, so trusted runner preparation cannot preinstall them. Its private
Bun cache starts empty; no node_modules or cache is committed.

SHA-256 of upstream inputs:

| File | SHA-256 |
| --- | --- |
| package.json | c32153f5e268c2680b31fa42f3627faf3462b3011bede209dd4b143df98ec816 |
| bun.lock | ea56ec3dd2c95aadc05ce0210b824e0683b8c86f2b90fc80c923cc581f579de3 |
| LICENSE | 139c2b5b5370aaf2bc08f0ef98bd90984c01ecec9448a6f93297f8ee28d2c938 |
