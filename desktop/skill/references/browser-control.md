# Screenshot-free browser workflow

Use for web forms, search, links, lists and application controls with accessible semantics. This drives an actual private Chromium browser through CDP input and reads its accessibility tree/rendered DOM; it does not require a site-specific integration. Use visual batch controls for canvas, custom widgets with missing semantics, layout judgments, browser chrome, or unsupported native apps.

## Start and observe

```bash
W=$(agent-desktop new --label "Web workflow" --maximize)
agent-desktop browser "$W" --start --url https://example.org
agent-desktop browser "$W" '[{"op":"inspect","query":"textbox|combobox|searchbox","limit":60}]'
```

`--start` launches through `agent-desktop run` into a dedicated disposable browser profile. It has no owner logins. Do not attach to an owner's profile, enable remote debugging on an existing browser, or use a shared-home workspace. The browser defaults to `--disable-gpu` for the private Xvnc display, avoiding GLX initialization failures. Debugging is loopback only. Subsequent calls reuse a pinned tab; a closed tab is an error rather than permission to switch silently. If the dedicated browser crashes, inspect its run log; close your workspace and recreate it rather than editing stale port files.

The command requires the workstation's installed Python `websockets` package and a supported Chromium browser (Brave, Chromium or Google Chrome). No screenshot is taken implicitly. Operations use **`op`**; native batch operations use **`action`**.

## Observe, batch predictable input, verify

Every target reference must come from an observation. References can expire after navigation or rerendering. Query filters match role, accessible name or value, with `|` separating alternatives. Discover search fields broadly with `query:"textbox|combobox|searchbox"`; sites expose different search roles. Apply an exact `role` filter only after observing the page’s actual role. Read a compact slice first; avoid dumping an entire document if a focused query is sufficient.

```json
[
  {"op":"fill","ref":"b123","text":"desired search"},
  {"op":"press","keys":["Return"]},
  {"op":"wait_url","url":"observed expected URL","contains":false},
  {"op":"inspect","query":"expected result|alert","limit":60}
]
```

Replace example refs and conditions with actually observed state. For a URL whose exact encoding is unknown, `wait_url` supports `contains:true` with a distinctive expected substring. Then inspect relevant content: URL readiness does not prove asynchronous results are complete. `wait` text already present before input does not prove a transition. Prefer a changed query value, URL, result or confirmation over fixed sleeps. Do not blindly retry partly executed submissions.

| Operation | Fields and result |
|---|---|
| `inspect` | Optional `query`, exact accessibility `role`, `contexts:true`, `limit` (default100, maximum1000), `scope` observed ref, or `main:true` for the main content landmark (falls back to root if absent). Returns URL/title/readiness, names/roles/values/state, refs and parent refs. `scope` limits to an observed subtree. `contexts:true` adds each returned node’s enclosing semantic row (text, full links, times); `context:null` means no row. Only the limited matches are resolved; use `role:"link"` to avoid duplicate static text matches. |
| `read` | `ref`: rendered element text, href and descendant links. |
| `context` | `ref`: closest semantic list/table row's rendered text, links and time elements, including open shadow DOM time text. Useful for collecting result title, URL and update together. Errors if no such ancestor exists; use scoped inspect or read. |
| `fill` | `ref`, `text`: raise the pinned browser page, focus the editable target, select all through Blink’s editing command, and replace through actual browser input. Verify exact value/text readback; a mismatch stops the batch without reinserting. |
| `click` | `ref`: scroll into view, check visibility/occlusion and dispatch mouse input. |
| `press` | `keys`: sequential key names or single-string chords, e.g. `["ctrl+a","BackSpace"]`, `["shift+Tab"]`. |
| `wait` | `query`, optional `timeout` (default8 seconds) and `limit`, `main`, `role`, `contexts`: wait for accessible text and return the matching observation with optional row context. |
| `wait_url` | `url`, optional `contains` (defaultfalse), `timeout` (default10 seconds). |
| `goto` | `url`: navigate, wait for document readiness and return final URL. Use supplied or observed destinations; use actual forms when the task requires them. |

Input is one JSON array as positional argument or stdin, up to100 operations. Output includes `completed`, `results`, `elapsed_ms`; failures include completed results and an error. Runtime errors can follow partial input, including the failing operation itself. Inspect before retrying. Input and submissions are never automatically replayed after an error or CDP timeout; the failed operation may already have changed the page.

## Collect results in the submission batch

After discovering the real search field, batch fill → Return → a distinctive URL transition → focused result observation. `inspect` and `wait` both accept `contexts:true`, so result titles, URLs and visible time text arrive together without another controller turn. For example, after a verified URL transition, use `{"op":"wait","query":"observed result term","role":"link","main":true,"contexts":true,"limit":3}`. Query terms use alternatives, not conjunctions; formulate the actual form search to enforce the task’s filters and ordering, then verify the returned rows. Set the limit to the number needed; contexts on a broad observation add unnecessary element resolutions. A wait returning any match does not prove all asynchronous results have arrived.

## What to optimize

Batch through the next genuine information dependency. Read links and row context directly instead of opening pages just to transcribe truncated titles or addresses. Check fresh error/confirmation state after submission. Forward screenshot images in the same tool response when visual fallback is needed, avoiding a separate model round trip solely to open a path.

Measure first observation to verified result, separately from helper execution time. The real GitHub issue-search trial with fresh GPT-6.1 Sol low controllers measured66.2 seconds visually versus28.4 seconds using structured controls, with matching three results; this is one matched task, not a general performance guarantee. The visual controller used7 screenshots; the structured controller used0. After URL waits and row context were added, a fresh controller took21.3 seconds and4 calls on that same task; browser startup was excluded in all trials. Re-test with the user's actual application and error/recovery cases before claiming broad gains.

Native desktop AT-SPI is a separate possible observation route. Its availability depends on the application and private accessibility bus; this browser feature does not imply native apps expose usable semantics.

Close only your workspace when done: `agent-desktop close "$W"`.

## Local control bindings

`find` resolves one current accessible control locally: `{"op":"find","role":"combobox","name":"Search issues","as":"search"}`. Exact role is required; exact name, observed `scope`, `main`, and `timeout` (default 3 seconds) are optional. Zero matches waits; ambiguity or truncated discovery stops before subsequent actions. Later operations in that request can use `"ref":"$search"` or `"scope":"$search"`; bindings are request-local and unique. Returned opaque refs remain ordinary observed refs. The whole request, including binding order, is validated before input.

Use a known semantic affordance when reliable (for example an app's search control); otherwise discover broadly first. This supports one observation/action/verification batch without hard-coded coordinates or manufactured refs. After navigation, resolve new controls rather than using old-page bindings.
