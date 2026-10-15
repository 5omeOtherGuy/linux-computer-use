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

impl Engine {
    fn authorize(&self, w: &WindowInfo) -> std::result::Result<(), Value> {
        if self.policy.denied(w) {
            return Err(
                json!({"code":"app_denied","message":"App is denied by policy; approval cannot override it"}),
            );
        }
        if !self.policy.allowed(w) {
            return Err(
                json!({"code":"approval_required","message":"User approval required before reading or controlling this app", "data":{"app_id":w.app_id,"window_id":w.window_id,"title":w.title}}),
            );
        }
        Ok(())
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
        if method == "turn_ended" {
            self.policy.reset();
            if let Some(ax) = &mut self.ax {
                ax.reset();
            }
            self.desktop.clear_input_abort()?;
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
        let w = self.desktop.window(id)?;
        if method == "approve_app" {
            if self.policy.denied(&w) {
                self.authorize(&w).map_err(ProtocolError)?;
            }
            if p.get("app_id").and_then(Value::as_str) != Some(&w.app_id) {
                bail!("App identity changed during approval");
            }
            self.policy.approve(&w);
            return Ok(json!({"approved":true}));
        }
        self.authorize(&w).map_err(ProtocolError)?;
        self.desktop.check_user_input()?;
        if method == "get_screenshot" {
            return self.desktop.screenshot(id);
        }
        if matches!(method, "get_app_state" | "get_ax_state") {
            return self.observe(&w, 0.0).await;
        }

        self.desktop.prepare_action(id)?;
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
            self.desktop.action(id, method, &params)?;
        } else {
            self.desktop.action(id, method, &r.params)?;
        }
        let start = Instant::now();
        let settled = self
            .desktop
            .settle(Duration::from_millis(35), Duration::from_millis(600))?;
        self.desktop.check_user_input()?;
        let current = self.desktop.window(id)?;
        if current.pid != w.pid || current.app_id != w.app_id {
            bail!("Window identity changed after action; outcome unknown");
        }
        self.authorize(&current).map_err(ProtocolError)?;
        let mut observation = self
            .observe(&current, start.elapsed().as_secs_f64() * 1000.0)
            .await?;
        if !settled {
            observation["warnings"]
                .as_array_mut()
                .unwrap()
                .push(json!("UI did not become quiet before settle deadline"));
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

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let display = std::env::var("CUA_DISPLAY").context("Set CUA_DISPLAY to an explicitly owned private X11 display; DISPLAY is never used implicitly")?;
    let desktop = if let Ok(ids) = std::env::var("CUA_AUTHORIZED_WINDOW_IDS") {
        let windows = ids
            .split(',')
            .map(str::parse::<u32>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let pid = std::env::var("CUA_AUTHORIZED_PID")
            .context("Scoped desktop access requires CUA_AUTHORIZED_PID")?
            .parse()?;
        Desktop::connect_authorized(&display, windows, pid)?
    } else {
        Desktop::connect(&display)?
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
