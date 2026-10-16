mod ax;
mod policy;
mod x11;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use x11::{Desktop, WindowInfo};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: Value,
    method: String,
    #[serde(default = "empty")]
    params: Value,
}
fn empty() -> Value {
    json!({})
}

struct Engine {
    desktop: Desktop,
    ax: Option<ax::Accessibility>,
    ax_error: String,
    ax_error_reported: bool,
    policy: policy::Policy,
    /// Window -> (verified PID, app ID) for every window resolved in this
    /// session, so an app-scoped request still finds its app after the
    /// window it names (e.g. a dialog) has closed.
    known: HashMap<u32, (u32, String)>,
    /// App -> window of its latest observation or screenshot: the frame of
    /// app-scoped coordinates.
    last_seen: HashMap<(u32, String), u32>,
}

/// Bound for the first sign that the app reacted to an action.
const SETTLE_REACT: Duration = Duration::from_millis(100);
/// Quiet interval after the last reaction.
const SETTLE_QUIET: Duration = Duration::from_millis(35);
const SETTLE_CAP: Duration = Duration::from_millis(600);

fn int_param(p: &serde_json::Map<String, Value>, k: &str) -> Option<u32> {
    p.get(k)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
}

impl Engine {
    fn authorize(&self, w: &WindowInfo) -> std::result::Result<(), Value> {
        match self.policy.decide(w) {
            policy::Decision::Allowed => Ok(()),
            policy::Decision::Denied(reason) => Err(
                json!({"code":"app_denied","message":format!("App is denied: {reason}; approval cannot override it")}),
            ),
            policy::Decision::NeedsApproval => Err(
                json!({"code":"approval_required","message":"User approval required before reading or controlling this app", "data":{"app_id":w.app_id,"window_id":w.window_id,"title":w.title,"executable":policy::executable(w.pid)}}),
            ),
        }
    }

    fn remember(&mut self, wins: &[WindowInfo]) {
        for w in wins {
            self.known.insert(w.window_id, (w.pid, w.app_id.clone()));
        }
    }

    /// (PID, app ID) of the app a window belongs or belonged to.
    fn app_of(&mut self, id: u32) -> Result<(u32, String)> {
        match self.desktop.window(id) {
            Ok(w) => {
                let app = (w.pid, w.app_id.clone());
                if self.known.get(&id).is_some_and(|k| *k != app) {
                    return Err(x11_error(
                        "window_not_found",
                        format!("window {id} now belongs to another app; list the windows again"),
                    ));
                }
                self.remember(std::slice::from_ref(&w));
                Ok(app)
            }
            Err(e) => self.known.get(&id).cloned().ok_or(e),
        }
    }

    async fn observe(&mut self, w: &WindowInfo, wins: &[WindowInfo], settle_ms: f64) -> Result<Value> {
        let start = Instant::now();
        let (text, mut warnings) = if let Some(ax) = &mut self.ax {
            let snapshot = ax.snapshot(w).await?;
            (snapshot.text, snapshot.warnings)
        } else if !self.ax_error_reported {
            self.ax_error_reported = true;
            (String::new(), vec![self.ax_error.clone()])
        } else {
            (String::new(), vec!["no accessibility tree (AT-SPI unavailable)".to_string()])
        };
        if text.is_empty() && warnings.is_empty() {
            warnings.push("no accessibility tree".into());
        }
        self.last_seen
            .insert((w.pid, w.app_id.clone()), w.window_id);
        let others: Vec<Value> = wins
            .iter()
            .filter(|o| o.window_id != w.window_id)
            .map(|o| json!({"window_id":o.window_id,"title":o.title,"transient_for":o.transient_for}))
            .collect();
        Ok(
            json!({"window":w,"accessibility":text,"warnings":warnings,"app_windows":others,
            "last_element_id":self.ax.as_ref().map(|a| a.last_element_id()),
            "virtual_cursor":self.desktop.virtual_cursor(w.window_id),
            "timing":{"settle_ms":settle_ms,"observation_ms":start.elapsed().as_secs_f64()*1000.0}}),
        )
    }

    async fn execute(&mut self, r: Request) -> std::result::Result<Value, Value> {
        self.execute_inner(r).await.map_err(|e| {
            if let Some(protocol) = e.downcast_ref::<ProtocolError>() {
                return protocol.0.clone();
            }
            let code = e
                .downcast_ref::<x11::X11Error>()
                .map(|e| e.code)
                .unwrap_or("operation_failed");
            json!({"code":code,"message":format!("{e:#}")})
        })
    }

    /// Runs a semantic action future while watching for physical user input.
    async fn with_input_watch<T>(
        desktop: &mut Desktop,
        action: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        tokio::pin!(action);
        let mut input_check = tokio::time::interval(Duration::from_millis(5));
        loop {
            tokio::select! {
                result = &mut action => return result,
                _ = input_check.tick() => desktop.check_user_input()?,
            }
        }
    }

    async fn execute_inner(&mut self, r: Request) -> Result<Value> {
        let p = r.params.as_object().context("params must be an object")?;
        let method = r.method.as_str();
        let keys: &[&str] =
            match method {
                "list_apps" | "list_windows" | "turn_ended" => &[],
                "approve_app" => &["window_id", "app_id"],
                "get_app_state" | "get_ax_state" | "get_screenshot" => &["window_id", "app"],
                "click" => &[
                    "window_id",
                    "app",
                    "element_index",
                    "x",
                    "y",
                    "button",
                    "click_count",
                ],
                "drag" => &["window_id", "app", "from", "to"],
                "scroll" => &[
                    "window_id",
                    "app",
                    "element_index",
                    "x",
                    "y",
                    "direction",
                    "pixels",
                ],
                "type_text" | "paste" => &["window_id", "app", "text"],
                "press_key" => &["window_id", "app", "key"],
                "set_value" => &["window_id", "app", "element_index", "value"],
                "perform_secondary_action" => &["window_id", "app", "element_index", "action"],
                _ => return Err(ProtocolError(
                    json!({"code":"unknown_method","message":format!("Unknown method {method}")}),
                )
                .into()),
            };
        if let Some(k) = p.keys().find(|k| !keys.contains(&k.as_str())) {
            return Err(ProtocolError(
                json!({"code":"invalid_params","message":format!("Unsupported parameter {k}")}),
            )
            .into());
        }
        if matches!(method, "click" | "scroll")
            && (p.contains_key("element_index") == (p.contains_key("x") || p.contains_key("y")))
        {
            return Err(ProtocolError(json!({"code":"invalid_params","message":"Provide exactly one target: element_index or x,y"})).into());
        }
        if matches!(method, "type_text" | "paste")
            && !p
                .get("text")
                .is_some_and(|t| t.as_str().is_some_and(|t| !t.is_empty()))
        {
            return Err(ProtocolError(json!({"code":"invalid_params","message":format!("{method} requires a non-empty string `text`")})).into());
        }
        let app_scope = match p.get("app") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(ProtocolError(
                    json!({"code":"invalid_params","message":"app must be a boolean"}),
                )
                .into())
            }
        };
        if method == "turn_ended" {
            self.policy.reset();
            if let Some(ax) = &mut self.ax {
                ax.reset();
            }
            self.desktop.end_turn()?;
            self.last_seen.clear();
            return Ok(json!({"reset":true}));
        }
        if matches!(method, "list_windows" | "list_apps") {
            let wins = self.desktop.list_windows()?;
            self.remember(&wins);
            return Ok(serde_json::to_value(wins)?);
        }
        let id: u32 = p
            .get("window_id")
            .and_then(Value::as_u64)
            .context("window_id must be an unsigned integer")?
            .try_into()?;
        if method == "approve_app" {
            let w = self.desktop.window(id)?;
            if let Err(denied @ Value::Object(_)) = self.authorize(&w) {
                if denied["code"] == "app_denied" {
                    return Err(ProtocolError(denied).into());
                }
            }
            if p.get("app_id").and_then(Value::as_str) != Some(&w.app_id) {
                bail!("App identity changed during approval");
            }
            // Approval is recorded only once the window can be read.
            self.desktop.track(&w)?;
            self.policy.approve(&w);
            self.remember(std::slice::from_ref(&w));
            return Ok(json!({"approved":true}));
        }

        // The target window is resolved once per request and passed down;
        // actions re-resolve once after settling. App-scoped requests name
        // any window of the app and act on the app's window that the input
        // belongs to: the window holding the element, the window last
        // observed (coordinates), or the app's focused/dialog window.
        let (app, wins, w) = if app_scope {
            let app = self.app_of(id)?;
            let wins = self.desktop.app_windows(app.0, &app.1)?;
            self.remember(&wins);
            if wins.is_empty() {
                return Err(x11_error(
                    "window_not_found",
                    format!("app {} has no open windows", app.1),
                ));
            }
            let element = int_param(p, "element_index")
                .and_then(|i| self.ax.as_ref().and_then(|ax| ax.window_of_element(i)))
                .and_then(|wid| wins.iter().find(|w| w.window_id == wid).cloned());
            let pointer = p.contains_key("x") || p.contains_key("from");
            let w = if let Some(w) = element {
                w
            } else if pointer && self.last_seen.contains_key(&app) {
                let seen = self.last_seen[&app];
                wins.iter()
                    .find(|w| w.window_id == seen)
                    .cloned()
                    .ok_or_else(|| x11_error("window_not_found", format!(
                        "window {seen}, which the coordinates refer to (the app's latest observation), has closed; observe the app again"
                    )))?
            } else {
                self.desktop
                    .active_window(&wins, id)
                    .expect("wins is not empty")
            };
            (app, wins, w)
        } else {
            let w = self.desktop.window(id)?;
            self.remember(std::slice::from_ref(&w));
            ((w.pid, w.app_id.clone()), vec![w.clone()], w)
        };
        self.authorize(&w).map_err(ProtocolError)?;
        self.desktop.check_user_input()?;
        if method == "get_screenshot" {
            let shot = self.desktop.screenshot(&w)?;
            self.last_seen.insert(app, w.window_id);
            return Ok(shot);
        }
        if matches!(method, "get_app_state" | "get_ax_state") {
            self.desktop.track(&w)?;
            return self.observe(&w, &wins, 0.0).await;
        }

        if matches!(method, "press_key" | "type_text" | "paste") {
            if let Some(ax) = self.ax.as_mut() {
                if let Some(reason) = ax.key_input_blocked(&w).await {
                    return Err(x11_error("input_refused", reason));
                }
            }
        }
        // Baseline for the app's accessibility events, before the action.
        let mut ax_activity = self.ax.as_ref().map(|ax| ax.activity_probe(&w));
        self.desktop.prepare_action(&w)?;
        let mut warnings: Vec<Value> = Vec::new();
        let element = int_param(p, "element_index");
        match (method, element) {
            ("click", Some(idx)) => {
                let ax = self.ax.as_mut().context(
                    "Accessibility unavailable: element actions require an isolated AT-SPI bus",
                )?;
                match ax.pointer_click_point(&w, idx).await? {
                    Some((sx, sy)) => {
                        let (ox, oy) = self.desktop.origin(w.window_id)?;
                        self.desktop.click_element_point(&w, sx - ox, sy - oy)?;
                    }
                    None => {
                        Self::with_input_watch(&mut self.desktop, ax.action(&w, method, &r.params))
                            .await?
                    }
                }
            }
            ("set_value" | "perform_secondary_action", _) => {
                let ax = self.ax.as_mut().context(
                    "Accessibility unavailable: element actions require an isolated AT-SPI bus",
                )?;
                let key_entry = match (method, element, p.get("value").and_then(Value::as_str)) {
                    ("set_value", Some(idx), Some(text)) => ax
                        .key_entry_point(&w, idx)
                        .await?
                        .map(|point| (idx, point, text)),
                    _ => None,
                };
                match key_entry {
                    // Web inputs in Chromium have no EditableText: focus the
                    // field, select its content and type the value.
                    Some((idx, (sx, sy), text)) => {
                        let (ox, oy) = self.desktop.origin(w.window_id)?;
                        self.desktop.click_element_point(&w, sx - ox, sy - oy)?;
                        let key = if text.is_empty() { "ctrl+a BackSpace" } else { "ctrl+a" };
                        for k in key.split(' ') {
                            self.desktop.action(
                                &w,
                                "press_key",
                                &json!({"window_id": w.window_id, "key": k}),
                            )?;
                        }
                        if !text.is_empty() {
                            self.desktop.type_keys(&w, text)?;
                        }
                        let deadline = Instant::now() + Duration::from_millis(400);
                        let got = loop {
                            let got = ax.element_text(&w, idx).await.unwrap_or_default();
                            if got == text || Instant::now() >= deadline {
                                break got;
                            }
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        };
                        if got != text {
                            warnings.push(json!(format!(
                                "typed the value as key events; the field now contains {:?}",
                                got.chars().take(120).collect::<String>()
                            )));
                        }
                    }
                    None => {
                        Self::with_input_watch(&mut self.desktop, ax.action(&w, method, &r.params))
                            .await?
                    }
                }
            }
            ("type_text" | "paste", _) => {
                let text = p["text"].as_str().unwrap_or_default();
                let outcome = match self.ax.as_mut() {
                    Some(ax) => {
                        Self::with_input_watch(&mut self.desktop, ax.text_input(&w, text, None))
                            .await?
                    }
                    None => ax::TextInput::NoTarget("AT-SPI is unavailable".into()),
                };
                match outcome {
                    ax::TextInput::Inserted(w) => warnings.extend(w.into_iter().map(Value::from)),
                    ax::TextInput::NoTarget(reason) => {
                        self.desktop.type_keys(&w, text)?;
                        warnings.push(json!(format!(
                            "typed as key events to the window's focus ({reason})"
                        )));
                    }
                }
            }
            ("scroll", Some(idx)) => {
                let ax = self.ax.as_mut().context("Accessibility unavailable")?;
                let (x, y) = ax.element_point(&w, idx).await?;
                let mut params = r.params.clone();
                let o = params.as_object_mut().unwrap();
                o.remove("element_index");
                o.remove("app");
                params["x"] = json!(x);
                params["y"] = json!(y);
                self.desktop.action(&w, method, &params)?;
            }
            _ => {
                let mut params = r.params.clone();
                params.as_object_mut().unwrap().remove("app");
                params["window_id"] = json!(w.window_id);
                self.desktop.action(&w, method, &params)?;
            }
        }
        let start = Instant::now();
        let mut external = || ax_activity.as_mut().is_some_and(|probe| probe());
        let settled = self
            .desktop
            .settle(SETTLE_REACT, SETTLE_QUIET, SETTLE_CAP, &mut external)?;
        self.desktop.check_user_input()?;
        // One re-check after settling: the window the result describes.
        let (after, wins) = if app_scope {
            let wins = self.desktop.app_windows(app.0, &app.1)?;
            self.remember(&wins);
            (self.desktop.active_window(&wins, w.window_id), wins)
        } else {
            match self.desktop.window(w.window_id) {
                Ok(c) => {
                    if c.pid != w.pid || c.app_id != w.app_id {
                        bail!("Window identity changed after action; outcome unknown");
                    }
                    (Some(c.clone()), vec![c])
                }
                Err(_) => (None, Vec::new()),
            }
        };
        let mut observation = match after {
            Some(current) => {
                self.authorize(&current).map_err(ProtocolError)?;
                self.observe(&current, &wins, start.elapsed().as_secs_f64() * 1000.0)
                    .await?
            }
            // The action closed the window (and, app-scoped, the app has no
            // other window): it happened; there is nothing left to observe.
            None => json!({"window":w,"closed":true,"accessibility":"","app_windows":[],
                "warnings":["the window closed after the action"],
                "timing":{"settle_ms":start.elapsed().as_secs_f64()*1000.0,"observation_ms":0.0}}),
        };
        if observation["window"]["window_id"] != json!(w.window_id) && !observation["closed"].as_bool().unwrap_or(false) {
            warnings.push(json!(format!(
                "the app's active window is now {} {:?}; the observation shows it",
                observation["window"]["window_id"], observation["window"]["title"].as_str().unwrap_or("")
            )));
        }
        if !settled.reacted {
            warnings.push(json!(format!(
                "No UI reaction (repaint, popup or accessibility event) within {} ms; the action may have had no effect, or the app may still respond",
                SETTLE_REACT.as_millis()
            )));
        } else if !settled.quiet {
            warnings.push(json!("UI did not become quiet before settle deadline"));
        }
        observation["warnings"]
            .as_array_mut()
            .expect("observation warnings")
            .extend(warnings);
        Ok(observation)
    }
}

fn x11_error(code: &'static str, message: String) -> anyhow::Error {
    anyhow::Error::new(x11::X11Error { code, message })
}

#[derive(Debug)]
struct ProtocolError(Value);
impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ProtocolError {}

const USAGE: &str = "usage: linux-cua [--first-element-id N] [--live-desktop --allow-app APP_ID [--allow-app APP_ID ...] (--pid PID | --allow-exe PATH) ...]";

#[derive(Debug)]
struct Args {
    live: Option<x11::LiveScope>,
    /// First element ID of this session; the host passes one above every ID
    /// an earlier daemon issued, so IDs never repeat across daemon restarts.
    first_element_id: u32,
}

/// Daemon command line. The live-desktop opt-in exists only here, never in
/// the environment, so it cannot be inherited from the MCP surface.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Args> {
    let mut live = false;
    let mut scope = x11::LiveScope::default();
    let mut first_element_id = 1;
    let mut args = args.peekable();
    while let Some(a) = args.next() {
        let mut value = || {
            args.next()
                .with_context(|| format!("{a} needs a value; {USAGE}"))
        };
        match a.as_str() {
            "--live-desktop" => live = true,
            "--first-element-id" => {
                first_element_id = value()?
                    .parse()
                    .context("--first-element-id must be a positive integer")?;
                if first_element_id == 0 {
                    bail!("--first-element-id must be positive");
                }
            }
            "--allow-app" => {
                let app = value()?.trim().to_lowercase();
                if app.is_empty() || app == "unknown" {
                    bail!("--allow-app needs an explicit WM_CLASS app ID");
                }
                scope.apps.insert(app);
            }
            "--pid" => {
                let pid: u32 = value()?.parse().context("--pid must be a process ID")?;
                if pid == 0 {
                    bail!("--pid must be nonzero");
                }
                scope.pids.insert(pid);
            }
            "--allow-exe" => {
                let path = value()?;
                let exe = std::fs::canonicalize(&path)
                    .with_context(|| format!("--allow-exe {path:?} is not an existing file"))?;
                scope.exes.insert(exe);
            }
            _ => bail!("unknown argument {a:?}; {USAGE}"),
        }
    }
    let scoped = !(scope.apps.is_empty() && scope.pids.is_empty() && scope.exes.is_empty());
    let live = match (live, scoped) {
        (false, false) => None,
        (false, true) => {
            bail!("--allow-app/--pid/--allow-exe are only valid with --live-desktop; {USAGE}")
        }
        // WM_CLASS alone is set by the client itself; the process must be
        // pinned by PID or executable too.
        (true, _) if scope.apps.is_empty() || (scope.pids.is_empty() && scope.exes.is_empty()) => {
            bail!("--live-desktop requires --allow-app and at least one --pid or --allow-exe; {USAGE}")
        }
        (true, _) => Some(scope),
    };
    Ok(Args {
        live,
        first_element_id,
    })
}

/// The former environment switch skipped the display check; refuse to start
/// rather than silently ignore a caller that still relies on it.
fn reject_retired_env(is_set: impl Fn(&str) -> bool) -> Result<()> {
    for retired in ["CUA_AUTHORIZED_WINDOW_IDS", "CUA_AUTHORIZED_PID"] {
        if is_set(retired) {
            bail!("{retired} is no longer supported; live desktop access is the explicit command-line opt-in --live-desktop --allow-app APP_ID");
        }
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = parse_args(std::env::args().skip(1))?;
    reject_retired_env(|k| std::env::var_os(k).is_some())?;
    let display = std::env::var("CUA_DISPLAY").context("Set CUA_DISPLAY to an explicitly owned private X11 display; DISPLAY is never used implicitly")?;
    let desktop = match args.live {
        Some(scope) => Desktop::connect_live(&display, scope)?,
        None => Desktop::connect(&display)?,
    };
    let (ax, ax_error) = match ax::Accessibility::connect(args.first_element_id).await {
        Ok(ax) => (Some(ax), String::new()),
        Err(e) => (None, format!("AT-SPI unavailable: {e:#}")),
    };
    let mut engine = Engine {
        desktop,
        ax,
        ax_error,
        ax_error_reported: false,
        policy: policy::Policy::from_env(),
        known: HashMap::new(),
        last_seen: HashMap::new(),
    };
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    loop {
        let mut line = Vec::new();
        let n = (&mut input)
            .take(1_048_577)
            .read_until(b'\n', &mut line)
            .await?;
        if n == 0 {
            break;
        }
        if n > 1_048_576 {
            bail!("RPC request exceeds 1 MiB; session closed");
        }
        let parsed: Result<Request, _> = serde_json::from_slice(&line);
        let response = match parsed {
            Err(e) => json!({"id":null,"error":{"code":"invalid_request","message":e.to_string()}}),
            Ok(request) => {
                let id = request.id.clone();
                // Best effort only: X11 waits block this thread, so the timer
                // fires only at the next await point. The real request
                // deadline is the trusted host, which SIGKILLs the daemon
                // (surface/src/engine.js) so a late action cannot land.
                match tokio::time::timeout(Duration::from_secs(20), engine.execute(request)).await {
                    Ok(Ok(result)) => json!({"id":id,"result":result}),
                    Ok(Err(error)) => json!({"id":id,"error":error}),
                    Err(_) => {
                        output.write_all(format!("{}\n",json!({"id":id,"error":{"code":"timeout","message":"Request timed out; action outcome unknown; session closed"}})).as_bytes()).await?;
                        output.flush().await?;
                        break;
                    }
                }
            }
        };
        output.write_all(format!("{response}\n").as_bytes()).await?;
        output.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(a: &[&str]) -> Result<Option<x11::LiveScope>> {
        parse_args(a.iter().map(|s| s.to_string())).map(|a| a.live)
    }
    #[test]
    fn live_desktop_is_an_explicit_scoped_flag() {
        assert!(args(&[]).unwrap().is_none());
        assert!(args(&["--live-desktop"]).is_err(), "allow list required");
        assert!(
            args(&["--allow-app", "Gedit"]).is_err(),
            "needs --live-desktop"
        );
        assert!(args(&["--live-desktop", "--allow-app", ""]).is_err());
        assert!(args(&["--live-desktop", "--allow-app", "x", "--pid", "0"]).is_err());
        assert!(args(&["--bogus"]).is_err());
        // Regression: WM_CLASS alone (client-set) used to be enough.
        assert!(args(&["--live-desktop", "--allow-app", "Gedit"]).is_err());
        assert!(args(&["--live-desktop", "--allow-app", "x", "--allow-exe", "/nonexistent/x"]).is_err());
        let s = args(&["--live-desktop", "--allow-app", "Gedit", "--pid", "42"])
            .unwrap()
            .unwrap();
        assert!(s.apps.contains("gedit") && s.pids.contains(&42));
        let s = args(&["--live-desktop", "--allow-app", "sh", "--allow-exe", "/bin/sh"])
            .unwrap()
            .unwrap();
        assert!(s.exes.contains(&std::fs::canonicalize("/bin/sh").unwrap()));
        let a = parse_args(["--first-element-id", "5000"].iter().map(|s| s.to_string())).unwrap();
        assert_eq!(a.first_element_id, 5000);
        assert!(parse_args(["--first-element-id", "0"].iter().map(|s| s.to_string())).is_err());
        // Regression: environment variables (inheritable from the MCP
        // surface) used to select the display-0 bypass.
        assert!(reject_retired_env(|k| k == "CUA_AUTHORIZED_WINDOW_IDS").is_err());
        assert!(reject_retired_env(|k| k == "CUA_AUTHORIZED_PID").is_err());
        assert!(reject_retired_env(|_| false).is_ok());
        assert!(Desktop::connect(":0").is_err());
    }
}
