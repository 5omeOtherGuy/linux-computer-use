# Measurements

Recorded benchmark results for Linux CUA. Each section names its date and method; the numbers are not re-measured when the code changes. How to run the benchmarks is in the [README](../README.md#on-a-private-display).

## Integration and benchmark results

The following measurements cover the earlier controller integration. Both components now live in this repository, with the workspace manager under `desktop/`:

| From | What |
|---|---|
| linux-cua | Rust X11/AT-SPI engine with an event-maintained tree mirror; MCP `js` REPL in the shape of OpenAI's `cua` API; per-app approval, frame checks and fail-closed text entry; diffs; provider adapters |
| agent-desktop | private workspaces (`CUA_WORKSPACE=new` creates one owned by the server); CDP browser control (`cua.getBrowser()`: AX snapshot, `setValue` with exact readback, click with scroll-into-view and occlusion check, `waitFor`, `read`, `goto`); locate-and-act in one request (`{role, name, text, states}` locators wherever an element ID is accepted, `app.find`) |
| new in the merge | settle that waits for the app's CPU after shortcuts and button clicks (a dialog the shortcut opens is ready for the next action of the cell); `getApp("Brave-browser")` returns the CDP tab when a browser is configured; one-time hint showing how to fill a dialog in the cell that opens it |

Measured with `benchmarks/run.mjs` (fresh private desktop per run, outcome verified from disk, GPT-6.1 Sol through `codex exec`, Opus 5.5 through `claude -p`, both at low effort, 3 runs per cell unless noted; median wall seconds, all runs passed). agent-desktop ran as a shell controller with its skill documentation (`--controller agent-desktop`); its native and paint rows ran earlier the same day (label `eval-ad`), all other rows interleaved (label `eval4`):

| task | model | agent-desktop | linux-cua 5201954 | merged e81dc9d |
|---|---|---|---|---|
| reimburse (Calculator, Xed, Thunar) | GPT | 60.7 | 56.8 | 53.7 |
| reimburse | Opus | 64.8 | 64.0 | 36.1 |
| xed-note | GPT | 34.2 | 27.2 | 24.7 |
| xed-note | Opus | 41.6 | 24.0 | 21.3 |
| webform (local web app, Brave) | GPT | 28.6 | 38.7 | 28.2 |
| webform | Opus | 42.5 | 30.5 (n=9) | 23.0 (n=9) |
| paint (GNOME Drawing) | GPT | 112.6 | 36.1 (n=9) | 32.8 (n=9) |
| paint | Opus | 42.3 | 38.2 | 27.9 |

Run-to-run spread is large (one merged paint run took 77.9 s); medians of 3 runs differ by several seconds between rounds. GPT's `service_tier="fast"` (Codex `-c service_tier="fast"`) cut reimburse from 48.1 to 38.4 s median over 3 interleaved pairs (2.2-2.3 s per model turn instead of 2.5-3.5 s); its effect on subscription usage was not measured.

## Standard benchmark

Measured on 2026-10-16 with the standard defaults, three fresh private desktops per label, release binaries, and the unchanged prompt/verifier:

| Revision / label | Pass | Wall seconds (all runs) | Median wall | Median calls | Median tool time |
|---|---|---|---|---|---|
| `1a7ce6c` / `speed4-before` | 3/3 | 107.8, 118.7, 98.7 | 107.8 s | 21 | 20.3 s |
| Continuous paths / `speed4-paths` | 1/3 | 50.2, 64.8, 54.8 | 64.8 s (passed only) | 15 | 8.9 s |
| Width/palette guidance / `speed4-final` | 3/3 | 75.4, 52.6, 57.8 | 57.8 s | 10 | 13.4 s |
| Numeric spin values / `speed4` | 3/3 | 101.5, 52.4, 62.4 | 62.4 s | 11 | 12.8 s |
| Delivered changes / `speed4-paced` | 3/3 | 70.7, 50.0, 34.0 | **50.0 s** | **10** | **8.2 s** |
| Modifiers, hover, launch, clipboard, table hit test (`cb3a5ac`) / `gaps` | 3/3 | 37.5, 45.9, 49.4 | 45.9 s | 9 | 8.2 s |

The `gaps` row is the same prompt and verifier after the round that added held modifiers, hover, `launchApp`, the clipboard, the table hit test and the one-request tree-plus-screenshot: no regression (the model used none of the new calls in those three runs, which drew with the shape and line tools as before; the pictures were checked visually).

The delivered combination reduced median wall time by **53.6%** (2.16× speedup). All three delivered runs had zero tool errors, passed the disk PNG verifier, and their saved images were visually checked for a house, roof, door, sun and multiple colours. The spread is wide: this is a three-run median on a shared workstation. The 70.7 s run repaired strokes near a newly visible scrollbar. Earlier path-only trials failed the colour-area check because their second-colour outlines were too thin; those failures remain in the results.

The changes combine continuous pointer paths, 20 ms vertex pacing to let GTK consume motion before release, drag timestamps that no longer advance by a fictitious 16 ms per burst sample, committed numeric spin values, and general canvas/tool/palette guidance for batching. No task-specific drawing macro or benchmark change is involved. Existing stale-element and window guards remain enforced.

Reproduce with `node benchmarks/run.mjs --label <label> --iterations 3 --engine <measurement-dir>/linux-cua` and `node benchmarks/report.mjs`. The baseline binary was `<measurement-dir>/baseline`. Run directories, binary hashes and all trial metrics were recorded in `<measurement-dir>/measurement-metadata.json` and raw results in `$BENCH_ROOT/results.jsonl` on the measuring machine; they are not part of this repository.

Validation: `cargo test --locked` 24/24; surface `npm test` 52 passed (Drawing opt-in skipped); private-display Drawing regression passed, including numeric size readback, all path edges, repeated straight drags and refusal before drawing an out-of-window path; the existing real-app suite passed 5/5 on the delivery binary. A cold Xed dialog race in that suite also occurred with the untouched baseline (4/5): Ctrl+O can outlast its existing reaction wait. Both logs are retained as `real-apps-baseline-check.log` and `real-apps-delivery-check.log` in the measurement directory. The suite and its assertions were not weakened.
