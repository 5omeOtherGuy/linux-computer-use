# AGENTS.md

## Profile
- Purpose: Linux desktop toolkit for computer-use agents: isolated X11 workspaces, a Rust X11/AT-SPI engine and an MCP surface
- Stage: prototype (repository standard 2026-10-18)
- Gate: `scripts/gate` (CI job `gate`)
- Landing: direct to main after a green gate; agents land: yes
- Release: none; agents release: no
- Outside issues and PRs: agents triage; outside PRs merge: owner approves
- Sensitive paths: `src/policy.rs`, `src/main.rs`, `src/launch.rs`, `src/clipboard.rs`, `surface/src/sandbox/`, `surface/src/host.js`, `surface/src/protocol.js`, `surface/src/engine.js`, `surface/src/browser/cdp.js`, `desktop/bin/`, `desktop/panel/install-panel.sh`, `scripts/install-desktop.py`, `CONTRACT.md`, `scripts/gate`, `.github/`, `AGENTS.md`

## Map

| Path | What |
|---|---|
| `src/` | Rust engine (`linux-cua`): NDJSON daemon on stdin/stdout. `main.rs` orchestration and authorization, `policy.rs` allow/deny/approval, `x11.rs` capture and input, `ax.rs` AT-SPI tree and actions, `launch.rs` desktop entries, `clipboard.rs` |
| `CONTRACT.md` | The engine's RPC contract and module boundaries; change it together with the code |
| `surface/` | Node MCP server (`bin/linux-cua-mcp.js`), QuickJS model sandbox (`src/sandbox/`), CDP browser control (`src/browser/`), provider adapters (`src/adapters/`), tests (`test/`) |
| `desktop/` | agent-desktop: workspace manager CLI (`bin/agent-desktop`), native/browser/fast controllers, XFCE panel item (`panel/`), agent skill (`skill/`), tests (`tests/`) |
| `scripts/` | `gate`, vendored `secret-scan.sh`, `install-desktop.py` (symlinks commands and the skill) |
| `tests/` | installer test |
| `bench/`, `benchmarks/` | engine micro-benchmarks and end-to-end agent benchmarks; results stay outside the repository |
| `docs/measurements.md` | recorded benchmark results |

## Landing

`prototype`: small commits straight to `main`. Run `scripts/gate` before pushing and push at the end of every session. Risky work goes on `spike/<slug>` branches. Conventional Commits subjects; author and committer `5omeOtherGuy <268151429+5omeOtherGuy@users.noreply.github.com>`. Replace or drop a `Co-authored-by:` trailer that carries a personal address before pushing.

Changes to the sensitive paths above get an independent review (another agent or the owner) before they land.

## Rules

- Clean-room: never inspect or copy proprietary computer-use implementations, binaries, packages or instruction text. Public API shapes and public documentation are the only inputs (see `CONTRACT.md`).
- Never point the engine, a test or a benchmark at display `:0`, the owner's session bus or another agent's workspace. Use a private display (`agent-desktop new --a11y`) and close only the workspace you created.
- The raw NDJSON API (including `approve_app`) is a trusted-host API. Never expose it to model code or a network socket; every model call goes through the surface's validation and approval.
- Deny always wins over allow and approval. Do not weaken a safety check or a test of one to make something pass.
- `scripts/gate` is hermetic: no display, credentials or real home. Display and live suites are opt-in and never part of the gate: `CUA_X11_TEST_DISPLAY=:N` (Rust tests on a private X server, e.g. `cargo test --locked x11::tests::clipboard`), `CUA_REAL_ENGINE` (`surface/test/real-apps.test.js`, `drawing.test.js`), `CUA_BROWSER_CDP` (`surface/test/browser.test.js`), `desktop/tests/test_agent_desktop.py` and `test_fast.py` (create real workspaces), `bench/integration.mjs`.
- `bench/model.mjs` makes paid provider API calls unless `--dry-run` is given; `benchmarks/run.mjs` starts real agents (`codex exec`, `claude -p`), with `--controller agent-desktop` in full-access mode. Run either only when the owner asks.
- On small machines build with `CARGO_BUILD_JOBS=2`.

## Focused tests

```sh
cargo test --locked x11::tests::display_zero_denied   # one Rust test
npm test --prefix surface                            # whole surface suite
node --test surface/test/repl.test.js                # one surface file
python3 -m unittest discover -s desktop/tests -p test_native.py
```
