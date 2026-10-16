mod ax;
mod policy;
mod x11;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
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
    policy: policy::Policy,
}

/// Bound for the first sign that the app reacted to an action.
const SETTLE_REACT: Duration = Duration::from_millis(100);
/// Quiet interval after the last reaction.
const SETTLE_QUIET: Duration = Duration::from_millis(35);
const SETTLE_CAP: Duration = Duration::from_millis(600);

impl Engine {
    fn authorize(&self, w: &WindowInfo) -> std::result::Result<(), Value> {
        match self.policy.decide(w) {
            policy::Decision::Allowed => Ok(()),
            policy::Decision::Denied(reason) => Err(
                json!({"code":"app_denied","message":format!("App is denied: {reason}; approval cannot override it")}),
            ),
            policy::Decision::NeedsApproval => Err(
                json!({"code":"approval_required","message":"User approval required before reading or controlling this app", "data":{"app_id":w.app_id,"window_id":w.window_id,"title":w.title}}),
            ),
        }
    }

    async fn observe(&mut self, w: &WindowInfo, settle_ms: f64) -> Result<Value> {
        let start = Instant::now();
        let (text, diff, warnings) = if let Some(ax) = &mut self.ax {
            let snapshot = ax.snapshot(w).await?;
            (snapshot.text, snapshot.diff, snapshot.warnings)
        } else {
            (String::new(), String::new(), vec![self.ax_error.clone()])
        };
        Ok(
            json!({"window":w,"accessibility":text,"ax_diff":diff,"warnings":warnings,
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

    async fn execute_inner(&mut self, r: Request) -> Result<Value> {
        let p = r.params.as_object().context("params must be an object")?;
        let method = r.method.as_str();
        let keys: &[&str] =
            match method {
                "list_apps" | "list_windows" | "turn_ended" => &[],
                "approve_app" => &["window_id", "app_id"],
                "get_app_state" | "get_ax_state" | "get_screenshot" => &["window_id"],
                "click" => &[
                    "window_id",
                    "element_index",
                    "x",
                    "y",
                    "button",
                    "click_count",
                ],
                "drag" => &["window_id", "from", "to"],
                "scroll" => &[
                    "window_id",
                    "element_index",
                    "x",
                    "y",
                    "direction",
                    "pixels",
                ],
                "type_text" | "paste" => &["window_id", "text"],
                "press_key" => &["window_id", "key"],
                "set_value" => &["window_id", "element_index", "value"],
                "perform_secondary_action" => &["window_id", "element_index", "action"],
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
        if method == "turn_ended" {
            self.policy.reset();
            if let Some(ax) = &mut self.ax {
                ax.reset();
            }
            self.desktop.end_turn()?;
            return Ok(json!({"reset":true}));
        }
        if matches!(method, "list_windows" | "list_apps") {
            return Ok(serde_json::to_value(self.desktop.list_windows()?)?);
        }
        let id: u32 = p
            .get("window_id")
            .and_then(Value::as_u64)
            .context("window_id must be an unsigned integer")?
            .try_into()?;
        // The window is resolved once per request and passed down; actions
        // re-check its identity once more after settling.
        let w = self.desktop.window(id)?;
        if method == "approve_app" {
            if let Err(denied @ Value::Object(_)) = self.authorize(&w) {
                if denied["code"] == "app_denied" {
                    return Err(ProtocolError(denied).into());
                }
            }
            if p.get("app_id").and_then(Value::as_str) != Some(&w.app_id) {
                bail!("App identity changed during approval");
            }
            self.policy.approve(&w);
            self.desktop.track(&w)?;
            return Ok(json!({"approved":true}));
        }
        self.authorize(&w).map_err(ProtocolError)?;
        self.desktop.check_user_input()?;
        if method == "get_screenshot" {
            return self.desktop.screenshot(&w);
        }
        if matches!(method, "get_app_state" | "get_ax_state") {
            self.desktop.track(&w)?;
            return self.observe(&w, 0.0).await;
        }

        // Baseline for the app's accessibility events, before the action.
        let mut ax_activity = self.ax.as_ref().map(|ax| ax.activity_probe(&w));
        self.desktop.prepare_action(&w)?;
        let semantic = matches!(
            method,
            "set_value" | "perform_secondary_action" | "type_text" | "paste"
        ) || (method == "click" && p.contains_key("element_index"));
        if semantic {
            let ax = self.ax.as_mut().context(
                "Accessibility unavailable: semantic actions require an isolated AT-SPI bus",
            )?;
            let action = ax.action(&w, method, &r.params);
            tokio::pin!(action);
            let mut input_check = tokio::time::interval(Duration::from_millis(5));
            loop {
                tokio::select! {
                    result = &mut action => { result?; break; },
                    _ = input_check.tick() => self.desktop.check_user_input()?,
                }
            }
        } else if method == "scroll" && p.contains_key("element_index") {
            let ax = self.ax.as_mut().context("Accessibility unavailable")?;
            let index = p["element_index"]
                .as_u64()
                .context("Invalid element_index")?;
            let (x, y) = ax.element_point(&w, index.try_into()?).await?;
            let mut params = r.params.clone();
            params.as_object_mut().unwrap().remove("element_index");
            params["x"] = json!(x);
            params["y"] = json!(y);
            self.desktop.action(&w, method, &params)?;
        } else {
            self.desktop.action(&w, method, &r.params)?;
        }
        let start = Instant::now();
        let mut external = || ax_activity.as_mut().is_some_and(|probe| probe());
        let settled = self
            .desktop
            .settle(SETTLE_REACT, SETTLE_QUIET, SETTLE_CAP, &mut external)?;
        self.desktop.check_user_input()?;
        let current = self.desktop.window(id)?;
        if current.pid != w.pid || current.app_id != w.app_id {
            bail!("Window identity changed after action; outcome unknown");
        }
        self.authorize(&current).map_err(ProtocolError)?;
        let mut observation = self
            .observe(&current, start.elapsed().as_secs_f64() * 1000.0)
            .await?;
        let warnings = observation["warnings"].as_array_mut().unwrap();
        if !settled.reacted {
            warnings.push(json!(format!(
                "No UI reaction (repaint, popup or accessibility event) within {} ms; the action may have had no effect, or the app may still respond",
                SETTLE_REACT.as_millis()
            )));
        } else if !settled.quiet {
            warnings.push(json!("UI did not become quiet before settle deadline"));
        }
        Ok(observation)
    }
}

#[derive(Debug)]
struct ProtocolError(Value);
impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ProtocolError {}

const USAGE: &str =
    "usage: linux-cua [--live-desktop --allow-app APP_ID [--allow-app APP_ID ...] [--pid PID ...]]";

/// Daemon command line. The live-desktop opt-in exists only here, never in
/// the environment, so it cannot be inherited from the MCP surface.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<x11::LiveScope>> {
    let mut live = false;
    let mut scope = x11::LiveScope::default();
    let mut args = args.peekable();
    while let Some(a) = args.next() {
        let mut value = || {
            args.next()
                .with_context(|| format!("{a} needs a value; {USAGE}"))
        };
        match a.as_str() {
            "--live-desktop" => live = true,
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
            _ => bail!("unknown argument {a:?}; {USAGE}"),
        }
    }
    match (live, scope.apps.is_empty() && scope.pids.is_empty()) {
        (false, true) => Ok(None),
        (false, false) => bail!("--allow-app/--pid are only valid with --live-desktop; {USAGE}"),
        (true, _) if scope.apps.is_empty() => {
            bail!("--live-desktop requires at least one --allow-app; {USAGE}")
        }
        (true, _) => Ok(Some(scope)),
    }
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
    let live = parse_args(std::env::args().skip(1))?;
    reject_retired_env(|k| std::env::var_os(k).is_some())?;
    let display = std::env::var("CUA_DISPLAY").context("Set CUA_DISPLAY to an explicitly owned private X11 display; DISPLAY is never used implicitly")?;
    let desktop = match live {
        Some(scope) => Desktop::connect_live(&display, scope)?,
        None => Desktop::connect(&display)?,
    };
    let (ax, ax_error) = match ax::Accessibility::connect().await {
        Ok(ax) => (Some(ax), String::new()),
        Err(e) => (None, format!("AT-SPI unavailable: {e:#}")),
    };
    let mut engine = Engine {
        desktop,
        ax,
        ax_error,
        policy: policy::Policy::from_env(),
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
        parse_args(a.iter().map(|s| s.to_string()))
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
        let s = args(&["--live-desktop", "--allow-app", "Gedit", "--pid", "42"])
            .unwrap()
            .unwrap();
        assert!(s.apps.contains("gedit") && s.pids.contains(&42));
        // Regression: environment variables (inheritable from the MCP
        // surface) used to select the display-0 bypass.
        assert!(reject_retired_env(|k| k == "CUA_AUTHORIZED_WINDOW_IDS").is_err());
        assert!(reject_retired_env(|k| k == "CUA_AUTHORIZED_PID").is_err());
        assert!(reject_retired_env(|_| false).is_ok());
        assert!(Desktop::connect(":0").is_err());
    }
}
