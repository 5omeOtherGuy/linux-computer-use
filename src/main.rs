mod ax;
mod clipboard;
mod launch;
mod policy;
mod x11;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use x11::{Desktop, HitArea, WindowInfo};

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
    /// Applications started with `launch_app`, reaped as they exit.
    children: Vec<std::process::Child>,
}

/// How long `launch_app` waits for the application's first window.
const LAUNCH_WAIT: Duration = Duration::from_secs(10);

/// Bound for the first sign that the app reacted to an action. A GTK app
/// that has to build a file chooser shows nothing for 70-100 ms on an idle
/// machine and longer on a loaded one; an action it actually ignored costs
/// this once.
const SETTLE_REACT: Duration = Duration::from_millis(200);
/// Quiet interval after the last reaction.
const SETTLE_QUIET: Duration = Duration::from_millis(35);
/// After a shortcut or a button click, which may open a dialog, the app's
/// CPU time counts as activity too: GTK builds a file chooser for 100-300 ms
/// without any X or accessibility event, while the focused window's own
/// redraw already counts as the reaction, so the quiet interval alone let
/// the next action reach the main window. CPU use below this per poll is
/// idle noise.
const BUSY_CPU_NS: u64 = 1_000_000;
const SETTLE_CAP: Duration = Duration::from_millis(600);
/// Longest wait, from the action, for a dialog to close after its confirm
/// key or button.
const DIALOG_CLOSE_WAIT: Duration = Duration::from_millis(1500);
/// A dialog that shows no activity for this long after settling is staying
/// open; the close wait ends. A dialog that stays open costs this once; one
/// that closes after a longer pause costs the model a turn.
const DIALOG_IDLE: Duration = Duration::from_millis(350);
/// ... and a dialog with no activity at all since settling, for this long
/// (the GTK file chooser opens a typed file 400-900 ms after Return, with
/// no activity before it unmaps, the longer on a loaded machine).
const DIALOG_STILL: Duration = Duration::from_millis(1000);
/// A dialog whose accessible frame is gone is closing: how long an
/// observation waits for its X window to unmap before showing the window
/// behind it.
const CLOSING_WAIT: Duration = Duration::from_millis(300);

/// Phase timings of requests and observations on stderr, when `CUA_TRACE`
/// is set in the environment.
pub(crate) fn trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CUA_TRACE").is_some())
}

/// Time since the daemon started (for ordering trace lines).
pub(crate) fn since_start() -> Duration {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

/// Named phases of one operation, printed as one line by `done`.
pub(crate) struct Trace {
    what: String,
    start: Instant,
    last: Instant,
    parts: Vec<String>,
}

impl Trace {
    pub(crate) fn new(what: impl Into<String>) -> Self {
        let now = Instant::now();
        Trace {
            what: what.into(),
            start: now,
            last: now,
            parts: Vec::new(),
        }
    }
    /// Records the time since the previous mark under `name`.
    pub(crate) fn mark(&mut self, name: &str) {
        let now = Instant::now();
        self.parts.push(format!(
            "{name}={:.1}",
            (now - self.last).as_secs_f64() * 1000.0
        ));
        self.last = now;
    }
    pub(crate) fn note(&mut self, name: &str, value: impl std::fmt::Display) {
        self.parts.push(format!("{name}={value}"));
    }
    pub(crate) fn done(self) {
        if trace_on() {
            eprintln!(
                "trace {} total={:.1} {}",
                self.what,
                self.start.elapsed().as_secs_f64() * 1000.0,
                self.parts.join(" ")
            );
        }
    }
}

/// A key chord with Ctrl, Alt or Super, or a function key: the keys that
/// open dialogs ("ctrl+shift+s", "F2"), unlike typing, arrows or Tab, and
/// unlike the editing chords (select all, clipboard, undo, moving the caret).
fn is_shortcut(key: &str) -> bool {
    let parts: Vec<String> = key
        .split('+')
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    let last = parts.last().map(String::as_str).unwrap_or_default();
    let fkey = last.len() > 1 && last.starts_with('f') && last[1..].parse::<u8>().is_ok();
    let editing = matches!(
        last,
        "a" | "c"
            | "x"
            | "v"
            | "z"
            | "y"
            | "home"
            | "end"
            | "left"
            | "right"
            | "up"
            | "down"
            | "backspace"
            | "delete"
            | "page_up"
            | "page_down"
            | "prior"
            | "next"
    );
    fkey || (parts.len() > 1
        && !editing
        && parts[..parts.len() - 1].iter().any(|m| {
            matches!(
                m.as_str(),
                "ctrl" | "control" | "alt" | "super" | "meta" | "win"
            )
        }))
}

/// CPU time of all threads of `pid` (nanoseconds; 0 when unreadable).
fn cpu_time_ns(pid: u32) -> u64 {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return 0;
    };
    tasks
        .flatten()
        .filter_map(|t| std::fs::read_to_string(t.path().join("schedstat")).ok())
        .filter_map(|s| s.split_whitespace().next()?.parse::<u64>().ok())
        .sum()
}

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

    async fn observe(
        &mut self,
        w: &WindowInfo,
        wins: &[WindowInfo],
        settle_ms: f64,
    ) -> Result<Value> {
        let start = Instant::now();
        let (text, mut warnings) = if let Some(ax) = &mut self.ax {
            let snapshot = ax.snapshot(w).await?;
            (snapshot.text, snapshot.warnings)
        } else if !self.ax_error_reported {
            self.ax_error_reported = true;
            (String::new(), vec![self.ax_error.clone()])
        } else {
            (
                String::new(),
                vec!["no accessibility tree (AT-SPI unavailable)".to_string()],
            )
        };
        if text.is_empty() && warnings.is_empty() {
            warnings.push("no accessibility tree".into());
        }
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

    /// Window list as `list_windows` reports it (with `focused`).
    fn window_list(&mut self) -> Result<Value> {
        let wins = self.desktop.list_windows()?;
        self.remember(&wins);
        let focused = self.desktop.focused_of(&wins);
        let mut v = serde_json::to_value(&wins)?;
        for w in v.as_array_mut().expect("window list") {
            if focused.is_some_and(|f| w["window_id"] == json!(f)) {
                w["focused"] = json!(true);
            }
        }
        Ok(v)
    }

    /// Starts an installed desktop application on this display and waits
    /// for its first window. The model names the application (desktop ID,
    /// menu name or program), never a command line; the deny lists apply,
    /// and reading or controlling the windows still needs approval.
    async fn launch(&mut self, query: &str) -> Result<Value> {
        if self.desktop.is_live() {
            return Err(x11_error(
                "unsupported",
                "launching applications is not available on a live desktop".into(),
            ));
        }
        let entries = launch::load(&launch::data_dirs());
        let entry = match launch::find(&entries, query) {
            launch::Lookup::Found(e) => e,
            launch::Lookup::Ambiguous(ids) => {
                return Err(x11_error(
                    "invalid_params",
                    format!("{query:?} matches several installed applications: {}; use one of these IDs", ids.join(", ")),
                ))
            }
            launch::Lookup::NotFound => {
                return Err(x11_error(
                    "not_found",
                    format!("no installed application matches {query:?} (a desktop entry ID such as org.gnome.Calculator, its last part, the menu name or the program name)"),
                ))
            }
        };
        if entry.terminal {
            return Err(ProtocolError(json!({"code":"app_denied","message":format!("{} runs in a terminal, which is denied", entry.name)})).into());
        }
        if let Some(reason) = self.policy.launch_denied(&entry.names()) {
            return Err(ProtocolError(
                json!({"code":"app_denied","message":format!("{}: {reason}", entry.name)}),
            )
            .into());
        }
        let before: HashSet<u32> = self
            .desktop
            .list_windows()?
            .iter()
            .map(|w| w.window_id)
            .collect();
        let mut cmd = std::process::Command::new(&entry.exec[0]);
        cmd.args(&entry.exec[1..])
            .env("DISPLAY", self.desktop.display())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(dir) = entry
            .path
            .as_ref()
            .filter(|p| std::path::Path::new(p).is_dir())
        {
            cmd.current_dir(dir);
        }
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| {
            x11_error(
                "launch_failed",
                format!("cannot start {} ({}): {e}", entry.name, entry.exec[0]),
            )
        })?;
        let pid = child.id();
        let start = Instant::now();
        let mut exited: Option<std::process::ExitStatus> = None;
        loop {
            if exited.is_none() {
                if let Ok(Some(status)) = child.try_wait() {
                    exited = Some(status);
                }
            }
            let wins = self.desktop.list_windows()?;
            self.remember(&wins);
            let mut warnings: Vec<String> = Vec::new();
            let mut mine: Vec<&WindowInfo> = wins.iter().filter(|w| w.pid == pid).collect();
            if mine.is_empty() {
                if let Some(status) = exited {
                    // The process handed over to a running instance (D-Bus
                    // activation, single-instance apps) and exited: the new
                    // window belongs to that instance.
                    mine = wins
                        .iter()
                        .filter(|w| !before.contains(&w.window_id) && w.transient_for.is_none())
                        .collect();
                    if !mine.is_empty() {
                        warnings.push(format!(
                            "the started process exited ({status}); the window belongs to another process of the app"
                        ));
                    } else if !status.success() {
                        return Err(x11_error(
                            "launch_failed",
                            format!("{} exited ({status}) before showing a window", entry.name),
                        ));
                    }
                }
            }
            if !mine.is_empty() {
                let ids: Vec<u32> = mine.iter().map(|w| w.window_id).collect();
                let mut list = self.window_list()?;
                list.as_array_mut().expect("window list").retain(|w| {
                    w["window_id"]
                        .as_u64()
                        .is_some_and(|id| ids.contains(&(id as u32)))
                });
                if exited.is_none() {
                    self.children.push(child);
                }
                return Ok(
                    json!({"app":entry.id,"name":entry.name,"pid":pid,"windows":list,"warnings":warnings,
                    "timing":{"launch_ms":start.elapsed().as_secs_f64()*1000.0}}),
                );
            }
            if start.elapsed() >= LAUNCH_WAIT {
                if exited.is_none() {
                    self.children.push(child);
                }
                return Err(x11_error(
                    "no_window",
                    format!(
                        "{} (pid {pid}) started but showed no window within {} s; it may still be starting: list the windows again later",
                        entry.name,
                        LAUNCH_WAIT.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Forgets launched processes that have exited.
    fn reap(&mut self) {
        self.children
            .retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
    }

    /// Adds a screenshot of the observed window to an observation
    /// (`screenshot: true`), unless the window closed.
    fn attach_screenshot(&mut self, observation: &mut Value) -> Result<()> {
        if observation["closed"].as_bool().unwrap_or(false) {
            return Ok(());
        }
        let Some(id) = observation["window"]["window_id"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
        else {
            return Ok(());
        };
        let w = self.desktop.window(id)?;
        let mut shot = self.desktop.screenshot(&w)?;
        shot["window_id"] = json!(id);
        observation["screenshot"] = shot;
        Ok(())
    }

    async fn execute(&mut self, r: Request) -> std::result::Result<Value, Value> {
        self.execute_inner(r).await.map_err(|e| {
            if let Some(protocol) = e.downcast_ref::<ProtocolError>() {
                return protocol.0.clone();
            }
            // The code travels separately; a bare engine error's message
            // does not repeat it.
            let (code, message) = match e.downcast_ref::<x11::X11Error>() {
                Some(x) if e.chain().count() == 1 => (x.code, x.message.clone()),
                Some(x) => (x.code, format!("{e:#}")),
                None => ("operation_failed", format!("{e:#}")),
            };
            json!({"code":code,"message":message})
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

    /// Text input (type_text, paste). Semantic insertion into the focused
    /// text field; in a single-line field one trailing newline becomes a
    /// Return after the text. Without a focused field: in a dialog its only
    /// text field, else refused; in a main window key events, but only to
    /// the window the model last saw. Key-typed text is never sent to an
    /// alert or to a dialog whose accessibility tree is unknown: text meant
    /// for another window must not answer an unexpected "Save changes?".
    async fn enter_text(
        &mut self,
        w: &WindowInfo,
        text: &str,
        frame: Option<u32>,
        via_clipboard: bool,
    ) -> Result<Vec<Value>> {
        let mut warnings = Vec::new();
        let outcome = match self.ax.as_mut() {
            Some(ax) => {
                Self::with_input_watch(&mut self.desktop, ax.text_input(w, text, None)).await?
            }
            None => ax::TextInput::NoTarget("AT-SPI is unavailable".into()),
        };
        let mut target = None;
        let reason = match outcome {
            ax::TextInput::Inserted(ws) => return Ok(ws.into_iter().map(Value::from).collect()),
            ax::TextInput::SingleLine => return self.enter_line(w, text, None).await,
            ax::TextInput::NoTarget(reason) => reason,
        };
        let ax = self.ax.as_ref();
        let dialog = w.transient_for.is_some() || ax.is_some_and(|ax| ax.window_is_alert(w));
        if dialog {
            let refuse = |why: String| {
                x11_error(
                    "input_refused",
                    format!("{why}; the text was not typed. Read the window (getAXState) first"),
                )
            };
            match ax.and_then(|ax| ax.window_role(w)) {
                None => {
                    return Err(refuse(format!(
                        "window {} {:?} is a dialog whose accessibility tree is unknown",
                        w.window_id, w.title
                    )))
                }
                Some(_) if ax.is_some_and(|ax| ax.window_is_alert(w)) => {
                    return Err(refuse(format!(
                        "window {} {:?} is an alert with no text field",
                        w.window_id, w.title
                    )))
                }
                Some(_) => {}
            }
            target = ax.and_then(|ax| ax.sole_text_field(w));
            if target.is_none() {
                return Err(refuse(format!(
                    "dialog {} {:?} has no text field reporting focus, and not exactly one text field ({reason}); click the field or use setValue",
                    w.window_id, w.title
                )));
            }
        }
        if let Some(idx) = target {
            // The dialog's only text field (GTK's file chooser location entry
            // does not report focus): inserted there semantically.
            let ax = self.ax.as_mut().expect("target came from AT-SPI");
            match Self::with_input_watch(&mut self.desktop, ax.text_input(w, text, Some(idx)))
                .await?
            {
                ax::TextInput::Inserted(ws) => warnings.extend(ws.into_iter().map(Value::from)),
                ax::TextInput::SingleLine => return self.enter_line(w, text, Some(idx)).await,
                ax::TextInput::NoTarget(r) => bail!("text field {idx}: {r}"),
            }
            return Ok(warnings);
        }
        if frame.is_some_and(|f| f != w.window_id) {
            return Err(x11_error("input_refused", format!(
                "the app's active window is now {} {:?}, not the window you last saw, and it has no focused text field; look at it first. The text was not typed",
                w.window_id, w.title
            )));
        }
        if via_clipboard && !self.desktop.is_live() {
            // `paste` without a text field to insert into pastes the way a
            // user would: through the clipboard, which also carries text
            // the keyboard map cannot type.
            self.desktop.clipboard_write(text)?;
            self.desktop.action(
                w,
                "press_key",
                &json!({"window_id": w.window_id, "key": "ctrl+v"}),
            )?;
            warnings.push(json!(format!(
                "pasted through the clipboard (ctrl+v) into the window's focus ({reason})"
            )));
        } else {
            self.desktop.type_keys(w, text)?;
            warnings.push(json!(format!(
                "typed as key events to the window's focus ({reason})"
            )));
        }
        // Readback where the app reports the focused text object.
        if let Some(ax) = self.ax.as_ref() {
            tokio::time::sleep(Duration::from_millis(30)).await;
            if let Some(got) = ax.focused_text(w).await {
                if !got.contains(text.trim_end_matches('\n')) {
                    warnings.push(json!(format!(
                        "the focused field now contains {:?}",
                        got.chars().take(120).collect::<String>()
                    )));
                    if let Some(h) = ax::locale_hint(w.pid, text) {
                        warnings.push(json!(h));
                    }
                }
            }
        }
        Ok(warnings)
    }

    /// Single-line field: the text with at most one trailing newline, which
    /// is pressed as Return after the insertion completed.
    async fn enter_line(
        &mut self,
        w: &WindowInfo,
        text: &str,
        idx: Option<u32>,
    ) -> Result<Vec<Value>> {
        let Some(line) = text.strip_suffix('\n').filter(|l| !l.contains('\n')) else {
            return Err(x11_error("invalid_params", "the focused field is single-line and the text has a line break inside; type the parts separately with pressKey(\"Tab\") or pressKey(\"Return\") between them".into()));
        };
        let mut warnings = Vec::new();
        if !line.is_empty() {
            let ax = self
                .ax
                .as_mut()
                .expect("single-line target came from AT-SPI");
            match Self::with_input_watch(&mut self.desktop, ax.text_input(w, line, idx)).await? {
                ax::TextInput::Inserted(ws) => warnings.extend(ws.into_iter().map(Value::from)),
                _ => bail!("the single-line field lost focus before the text was inserted; nothing was typed"),
            }
        }
        // InsertText returned after the app applied it; the Return follows.
        self.desktop.action(
            w,
            "press_key",
            &json!({"window_id": w.window_id, "key": "Return"}),
        )?;
        Ok(warnings)
    }

    async fn execute_inner(&mut self, r: Request) -> Result<Value> {
        let p = r.params.as_object().context("params must be an object")?;
        let method = r.method.as_str();
        self.reap();
        let keys: &[&str] =
            match method {
                "list_apps" | "list_windows" | "turn_ended" | "clipboard_read"
                | "clipboard_release" => &[],
                "launch_app" => &["app"],
                "clipboard_write" => &["text"],
                "approve_app" => &["window_id", "app_id"],
                "get_screenshot" => &["window_id", "app"],
                "get_app_state" | "get_ax_state" => &["window_id", "app", "screenshot"],
                "click" => &[
                    "window_id",
                    "app",
                    "frame_window",
                    "observe",
                    "screenshot",
                    "element_index",
                    "x",
                    "y",
                    "button",
                    "click_count",
                    "modifiers",
                    "hold_ms",
                ],
                "drag" => &[
                    "window_id",
                    "app",
                    "frame_window",
                    "observe",
                    "screenshot",
                    "from",
                    "to",
                    "path",
                    "modifiers",
                ],
                "scroll" => &[
                    "window_id",
                    "app",
                    "frame_window",
                    "observe",
                    "screenshot",
                    "element_index",
                    "x",
                    "y",
                    "direction",
                    "pixels",
                    "modifiers",
                ],
                "mouse_move" => &[
                    "window_id",
                    "app",
                    "frame_window",
                    "observe",
                    "screenshot",
                    "element_index",
                    "x",
                    "y",
                    "dx",
                    "dy",
                ],
                "type_text" | "paste" => &[
                    "window_id",
                    "app",
                    "frame_window",
                    "observe",
                    "screenshot",
                    "text",
                ],
                "press_key" => &[
                    "window_id",
                    "app",
                    "observe",
                    "screenshot",
                    "key",
                    "hold_ms",
                ],
                "set_value" => &[
                    "window_id",
                    "app",
                    "observe",
                    "screenshot",
                    "element_index",
                    "value",
                ],
                "perform_secondary_action" => &[
                    "window_id",
                    "app",
                    "observe",
                    "screenshot",
                    "element_index",
                    "action",
                ],
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
        if method == "mouse_move" {
            let targets = [
                p.contains_key("element_index"),
                p.contains_key("x") || p.contains_key("y"),
                p.contains_key("dx") || p.contains_key("dy"),
            ];
            if targets.iter().filter(|t| **t).count() != 1 {
                return Err(ProtocolError(json!({"code":"invalid_params","message":"Provide exactly one target: element_index, x,y or dx,dy"})).into());
            }
        }
        if matches!(method, "type_text" | "paste" | "clipboard_write")
            && !p
                .get("text")
                .is_some_and(|t| t.as_str().is_some_and(|t| !t.is_empty()))
        {
            return Err(ProtocolError(json!({"code":"invalid_params","message":format!("{method} requires a non-empty string `text`")})).into());
        }
        match method {
            "launch_app" => {
                let name = p.get("app").and_then(Value::as_str).unwrap_or_default();
                if name.trim().is_empty() || name.len() > 256 {
                    return Err(ProtocolError(json!({"code":"invalid_params","message":"launch_app requires `app`, the application's name or desktop entry ID"})).into());
                }
                return self.launch(name).await;
            }
            "clipboard_write" => {
                self.desktop
                    .clipboard_write(p["text"].as_str().unwrap_or_default())?;
                return Ok(json!({"written":true}));
            }
            "clipboard_read" => {
                let text = self.desktop.clipboard_read()?;
                return Ok(json!({"text":text}));
            }
            "clipboard_release" => {
                self.desktop.clipboard_release()?;
                return Ok(json!({"released":true}));
            }
            _ => {}
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
        // `observe: false`: an action returns without the post-action tree
        // (the caller observes once at the end of a sequence).
        let observe = match p.get("observe") {
            None => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(ProtocolError(
                    json!({"code":"invalid_params","message":"observe must be a boolean"}),
                )
                .into())
            }
        };
        // `screenshot: true`: the observation also carries a screenshot of
        // the observed window (one request for tree and image).
        let screenshot = match p.get("screenshot") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(ProtocolError(
                    json!({"code":"invalid_params","message":"screenshot must be a boolean"}),
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
            return Ok(json!({"reset":true}));
        }
        if matches!(method, "list_windows" | "list_apps") {
            return self.window_list();
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

        let mut trace = Trace::new(format!("request {method}"));
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
            let element = match int_param(p, "element_index").and_then(|i| {
                self.ax.as_ref().and_then(|ax| ax.window_of_element(i)).map(|wid| (i, wid))
            }) {
                Some((i, wid)) => match wins.iter().find(|w| w.window_id == wid) {
                    Some(w) => Some(w.clone()),
                    // Still listed but not one of this app's windows.
                    None if self.desktop.window(wid).is_ok() => {
                        return Err(x11_error("invalid_params", format!(
                            "element {i} belongs to window {wid} of another app; use that app's object"
                        )))
                    }
                    None => {
                        return Err(x11_error("window_not_found", format!(
                            "element {i} was in window {wid}, which has closed; the app's current state is in the latest observation, or observe the app again"
                        )))
                    }
                },
                None => None,
            };
            let pointer = ["x", "from", "path", "dx"]
                .iter()
                .any(|k| p.contains_key(*k));
            let active = self
                .desktop
                .active_window(&wins, id)
                .expect("wins is not empty");
            let w = if let Some(w) = element {
                w
            } else if pointer {
                // Coordinates refer to the window the model last saw (the
                // surface passes it); never to one only an implicit
                // observation inside the same cell showed.
                let frame = int_param(p, "frame_window").unwrap_or(id);
                coordinate_window(frame, &wins, &active)?
            } else {
                active
            };
            (app, wins, w)
        } else {
            let w = self.desktop.window(id)?;
            self.remember(std::slice::from_ref(&w));
            ((w.pid, w.app_id.clone()), vec![w.clone()], w)
        };
        self.authorize(&w).map_err(ProtocolError)?;
        self.desktop.check_user_input()?;
        trace.mark("resolve");
        if method == "get_screenshot" {
            let mut shot = self.desktop.screenshot(&w)?;
            shot["window_id"] = json!(w.window_id);
            trace.mark("capture");
            trace.done();
            return Ok(shot);
        }
        if matches!(method, "get_app_state" | "get_ax_state") {
            self.desktop.track(&w)?;
            let scope = app_scope.then_some(&app);
            let mut o = self
                .observe_following(w.clone(), wins, scope, id, Instant::now(), &mut || false)
                .await?;
            trace.mark("observe");
            if screenshot {
                self.attach_screenshot(&mut o)?;
                trace.mark("capture");
            }
            trace.done();
            return Ok(o);
        }

        let mut warnings: Vec<Value> = Vec::new();
        if matches!(method, "press_key" | "type_text" | "paste") {
            if let Some(ax) = self.ax.as_mut() {
                match ax.key_input_gate(&w).await {
                    Ok(warning) => warnings.extend(warning.map(Value::from)),
                    Err(reason) => return Err(x11_error("input_refused", reason)),
                }
            }
        }
        trace.mark("gate");
        // Baseline for the app's accessibility events, before the action.
        if let Some(ax) = self.ax.as_mut() {
            ax.action_warnings.clear();
            ax.begin_action();
        }
        let mut ax_activity = self.ax.as_ref().map(|ax| ax.activity_probe(&w));
        self.desktop.prepare_action(&w)?;
        let element = int_param(p, "element_index");
        match (method, element) {
            ("click", Some(idx)) => {
                let ax = self.ax.as_mut().context(
                    "Accessibility unavailable: element actions require an isolated AT-SPI bus",
                )?;
                let plan = ax.click_plan(&w, idx).await?;
                let point = match plan {
                    ax::ClickPlan::Pointer { x, y, popup } => {
                        let (ox, oy) = self.desktop.origin(w.window_id)?;
                        let (x, y) = (x - ox, y - oy);
                        if let Some(rect) = popup {
                            Some((x, y, HitArea::Popup(rect)))
                        } else if (0..w.width as i32).contains(&x)
                            && (0..w.height as i32).contains(&y)
                        {
                            Some((x, y, HitArea::Body))
                        } else {
                            None
                        }
                    }
                    ax::ClickPlan::Action { warning } => {
                        warnings.extend(warning.map(Value::from));
                        None
                    }
                };
                match point {
                    Some((x, y, area)) => {
                        // The pointer click carries the requested button and
                        // count (right-click opens a context menu, not the
                        // left-click handler).
                        let mut click = json!({"x": x, "y": y});
                        for k in ["button", "click_count", "modifiers", "hold_ms"] {
                            if let Some(v) = p.get(k) {
                                click[k] = v.clone();
                            }
                        }
                        self.desktop.click_element_point(&w, &click, area)?
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
                        self.desktop.check_typeable(text)?;
                        let (ox, oy) = self.desktop.origin(w.window_id)?;
                        self.desktop.click_element_point(
                            &w,
                            &json!({"x": sx - ox, "y": sy - oy}),
                            HitArea::Body,
                        )?;
                        // The click may have hit an overlay or a read-only
                        // input: Ctrl+A and the value would then go to another
                        // field or trigger page shortcuts.
                        if !ax.focused_writable(&w, idx).await? {
                            return Err(x11_error(
                                "input_refused",
                                format!("clicking element {idx} did not give it focus as a writable field (covered by another element, or read-only); nothing was typed"),
                            ));
                        }
                        let key = if text.is_empty() {
                            "ctrl+a BackSpace"
                        } else {
                            "ctrl+a"
                        };
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
                let frame = int_param(p, "frame_window");
                let w2 = w.clone();
                warnings.extend(self.enter_text(&w2, text, frame, method == "paste").await?);
            }
            ("scroll" | "mouse_move", Some(idx)) => {
                let ax = self.ax.as_mut().context("Accessibility unavailable")?;
                let (x, y) = ax.element_point(&w, idx).await?;
                let mut params = r.params.clone();
                let o = params.as_object_mut().unwrap();
                o.remove("element_index");
                o.remove("app");
                o.remove("frame_window");
                o.remove("observe");
                o.remove("screenshot");
                params["x"] = json!(x);
                params["y"] = json!(y);
                self.desktop.action(&w, method, &params)?;
            }
            _ => {
                // `app`, `frame_window`, `observe` and `screenshot` were
                // resolved above.
                let mut params = r.params.clone();
                let o = params.as_object_mut().unwrap();
                o.remove("app");
                o.remove("frame_window");
                o.remove("observe");
                o.remove("screenshot");
                params["window_id"] = json!(w.window_id);
                self.desktop.action(&w, method, &params)?;
            }
        }
        if let Some(ax) = self.ax.as_mut() {
            warnings.extend(ax.action_warnings.drain(..).map(Value::from));
        }
        trace.mark("act");
        let start = Instant::now();
        let may_open = match (method, element) {
            ("press_key", _) => p
                .get("key")
                .and_then(Value::as_str)
                .is_some_and(is_shortcut),
            ("click", Some(idx)) => self.ax.as_ref().is_some_and(|ax| ax.element_is_button(idx)),
            _ => false,
        };
        let mut cpu = may_open.then(|| cpu_time_ns(w.pid));
        let mut external = || {
            let ax = ax_activity.as_mut().is_some_and(|probe| probe());
            let busy = cpu.as_mut().is_some_and(|last| {
                let now = cpu_time_ns(w.pid);
                let busy = now.saturating_sub(*last) >= BUSY_CPU_NS;
                *last = now;
                busy
            });
            ax || busy
        };
        let settled = self
            .desktop
            .settle(SETTLE_REACT, SETTLE_QUIET, SETTLE_CAP, &mut external)?;
        trace.mark("settle");
        trace.note("reacted", settled.reacted);
        // A dialog confirmed or dismissed (Return, Escape, a button) often
        // closes only after a file check or similar, past the quiet window;
        // wait for that so the next action and the observation see the app's
        // next window rather than the closing dialog.
        let confirms = match (method, element) {
            ("press_key", _) => p.get("key").and_then(Value::as_str).is_some_and(|k| {
                matches!(
                    k.trim().to_ascii_lowercase().as_str(),
                    "return" | "enter" | "kp_enter" | "escape" | "esc"
                )
            }),
            ("click", Some(idx)) => self.ax.as_ref().is_some_and(|ax| ax.element_is_button(idx)),
            ("type_text" | "paste", _) => p
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|t| t.ends_with('\n')),
            _ => false,
        };
        if w.transient_for.is_some() && confirms {
            let left = DIALOG_CLOSE_WAIT.saturating_sub(start.elapsed());
            let closed = self.desktop.wait_closed(
                w.window_id,
                left,
                DIALOG_IDLE,
                DIALOG_STILL,
                &mut external,
            )?;
            if closed {
                self.desktop
                    .settle(SETTLE_REACT, SETTLE_QUIET, SETTLE_CAP, &mut external)?;
            }
            trace.mark("dialog");
            trace.note("closed", closed);
        }
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
                Err(e)
                    if e.downcast_ref::<x11::X11Error>()
                        .is_some_and(|x| x.code == "window_not_found") =>
                {
                    (None, Vec::new())
                }
                Err(e) => return Err(e),
            }
        };
        trace.mark("recheck");
        let mut observation = match after {
            Some(current) if !observe => {
                // The caller observes later (the end of a cell with several
                // actions): the result names the window the action landed
                // on and the app's windows after it, without a tree.
                self.authorize(&current).map_err(ProtocolError)?;
                let others: Vec<Value> = wins
                    .iter()
                    .filter(|o| o.window_id != current.window_id)
                    .map(|o| json!({"window_id":o.window_id,"title":o.title,"transient_for":o.transient_for}))
                    .collect();
                json!({"window":current,"observed":false,"warnings":[],"app_windows":others,
                    "timing":{"settle_ms":start.elapsed().as_secs_f64()*1000.0,"observation_ms":0.0}})
            }
            Some(current) => {
                let scope = app_scope.then_some(&app);
                let o = self
                    .observe_following(current, wins, scope, w.window_id, start, &mut external)
                    .await?;
                trace.mark("observe");
                o
            }
            // The action closed the window (and, app-scoped, the app has no
            // other window): it happened; there is nothing left to observe.
            None => closed_result(&w, start),
        };
        if observation["window"]["window_id"] != json!(w.window_id)
            && !observation["closed"].as_bool().unwrap_or(false)
        {
            warnings.push(json!(format!(
                "the app's active window is now {} {:?}; the observation shows it",
                observation["window"]["window_id"],
                observation["window"]["title"].as_str().unwrap_or("")
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
        if screenshot {
            self.attach_screenshot(&mut observation)?;
            trace.mark("capture");
        }
        trace.done();
        Ok(observation)
    }
}

impl Engine {
    /// Observes `w` (the app's active window), following a dialog that is
    /// closing: its accessible frame goes before its X window unmaps, so a
    /// dialog without a tree is given `CLOSING_WAIT` to unmap, and the
    /// app's window behind it is observed instead. `scope` is the app of an
    /// app-scoped request (only then is there a window behind to follow).
    async fn observe_following(
        &mut self,
        w: WindowInfo,
        wins: Vec<WindowInfo>,
        scope: Option<&(u32, String)>,
        preferred: u32,
        start: Instant,
        external: &mut dyn FnMut() -> bool,
    ) -> Result<Value> {
        self.authorize(&w).map_err(ProtocolError)?;
        let settle_ms = |start: Instant| start.elapsed().as_secs_f64() * 1000.0;
        let o = self.observe(&w, &wins, settle_ms(start)).await?;
        let Some(app) = scope else { return Ok(o) };
        if w.transient_for.is_none() || o["accessibility"] != "" {
            return Ok(o);
        }
        if !self.desktop.wait_closed(
            w.window_id,
            CLOSING_WAIT,
            DIALOG_IDLE,
            CLOSING_WAIT,
            external,
        )? {
            return Ok(o);
        }
        self.desktop
            .settle(SETTLE_REACT, SETTLE_QUIET, SETTLE_CAP, external)?;
        let wins = self.desktop.app_windows(app.0, &app.1)?;
        self.remember(&wins);
        match self.desktop.active_window(&wins, preferred) {
            Some(behind) => {
                self.authorize(&behind).map_err(ProtocolError)?;
                self.observe(&behind, &wins, settle_ms(start)).await
            }
            None => Ok(closed_result(&w, start)),
        }
    }
}

/// Result of an action whose window closed, when the app has no other
/// window: it happened; there is nothing left to observe.
fn closed_result(w: &WindowInfo, start: Instant) -> Value {
    json!({"window":w,"closed":true,"accessibility":"","app_windows":[],
        "warnings":["the window closed after the action"],
        "timing":{"settle_ms":start.elapsed().as_secs_f64()*1000.0,"observation_ms":0.0}})
}

/// Window that app-scoped coordinates act on: `frame`, the window of the
/// last screenshot or tree the model received. Refused when it has closed,
/// or when the app's active window is another one (a dialog the action
/// before opened, which the model has not seen): the point would land on
/// whatever that window shows now.
fn coordinate_window(frame: u32, wins: &[WindowInfo], active: &WindowInfo) -> Result<WindowInfo> {
    let Some(w) = wins.iter().find(|w| w.window_id == frame) else {
        return Err(x11_error(
            "window_not_found",
            format!("window {frame}, which the coordinates refer to (the last state you received), has closed; look at the app again (getScreenshot or getAXState) before using coordinates"),
        ));
    };
    if active.window_id != frame {
        return Err(x11_error(
            "input_refused",
            format!(
                "the app's active window is now {} {:?}, not window {frame} that the coordinates refer to; look at it (getScreenshot or getAXState) before using coordinates. Nothing was clicked",
                active.window_id, active.title
            ),
        ));
    }
    Ok(w.clone())
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
        children: Vec::new(),
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
    fn win(window_id: u32, title: &str) -> WindowInfo {
        WindowInfo {
            window_id,
            title: title.into(),
            app_id: "Xed".into(),
            pid: 7,
            width: 100,
            height: 100,
            transient_for: (window_id != 1).then_some(1),
            origin: (0, 0),
        }
    }

    /// Regression: coordinates went to the window of the latest observation
    /// of any kind, including the implicit one after an action that opened
    /// a dialog the model never saw, so `click([500,40]); click([300,200])`
    /// sent the second click into that dialog. They now refer to the window
    /// the model last received and are refused when the app's active window
    /// changed since.
    #[test]
    fn coordinates_never_land_in_an_unseen_window() {
        let (main, dialog) = (win(1, "doc"), win(2, "Replace?"));
        let wins = vec![main.clone(), dialog.clone()];
        let e = coordinate_window(1, &wins, &dialog).unwrap_err();
        assert_eq!(
            e.downcast_ref::<x11::X11Error>().unwrap().code,
            "input_refused"
        );
        assert!(e.to_string().contains("\"Replace?\""), "{e}");
        assert_eq!(coordinate_window(2, &wins, &dialog).unwrap().window_id, 2);
        assert_eq!(
            coordinate_window(1, &wins[..1], &main).unwrap().window_id,
            1
        );
        // The window the model saw has closed.
        let e = coordinate_window(2, &wins[..1], &main).unwrap_err();
        assert_eq!(
            e.downcast_ref::<x11::X11Error>().unwrap().code,
            "window_not_found"
        );
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
        assert!(args(&[
            "--live-desktop",
            "--allow-app",
            "x",
            "--allow-exe",
            "/nonexistent/x"
        ])
        .is_err());
        let s = args(&["--live-desktop", "--allow-app", "Gedit", "--pid", "42"])
            .unwrap()
            .unwrap();
        assert!(s.apps.contains("gedit") && s.pids.contains(&42));
        let s = args(&[
            "--live-desktop",
            "--allow-app",
            "sh",
            "--allow-exe",
            "/bin/sh",
        ])
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
