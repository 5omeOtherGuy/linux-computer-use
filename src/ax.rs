//! AT-SPI accessibility backend.
//!
//! Clean-room implementation over zbus, written against the public AT-SPI2
//! D-Bus interface descriptions (at-spi2-core `xml/*.xml`) and the public
//! `atspi-constants.h` role/state enumerations.
//!
//! Behaviour summary:
//! * Bus: `AT_SPI_BUS_ADDRESS` if set, otherwise `org.a11y.Bus.GetAddress` on the
//!   explicit `DBUS_SESSION_BUS_ADDRESS`. No autolaunch, no X11 root-property
//!   discovery, and the owner's default `/run/user/<uid>/bus` session bus or
//!   `/run/user/<uid>/at-spi/bus*` accessibility bus are refused.
//! * Window mapping: registry applications whose D-Bus connection PID equals the
//!   window PID; their showing top-level children are scored by title, size and
//!   position. Ties or insufficient evidence are rejected as ambiguous.
//! * Tree: bulk `org.a11y.atspi.Cache.GetItems` (both current and legacy
//!   signatures), invalidated by any event from that application, by cache
//!   signals and by a maximum age. Holes and cache-less applications are filled
//!   with a bounded live traversal (node, depth, fan-out and time limits).
//! * IDs: stable per window while the same accessible object (bus name, path,
//!   role) exists. One counter serves the whole daemon session and is never
//!   reset, so an old ID can never come to mean a different element.
//! * Actions: every element action revalidates the element live (not defunct,
//!   same role and name, still showing, still a descendant of the mapped frame,
//!   not removed by an event after the observation). This module provides the
//!   semantic operations (actions, values, selection, text insertion) and the
//!   facts the engine needs to choose input: an element's visible centre, its
//!   focus and writability, the focused text field. The engine (`main.rs`) then
//!   clicks visible elements with the pointer instead of DoAction (toolkits run
//!   DoAction inside the accessibility call, and a handler that opens a modal
//!   dialog blocks the app), and types key events where no editable text has
//!   focus, under the checks documented there. A failed semantic operation is
//!   reported; it never falls back to an arbitrary other action or input.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::stream::{self, StreamExt};
use serde_json::Value;
use zbus::message::Type as MsgType;
use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, Message, MessageStream};

use crate::x11::WindowInfo;

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

const CALL_TIMEOUT: Duration = Duration::from_millis(1500);
const BULK_TIMEOUT: Duration = Duration::from_secs(4);
const ACTION_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Time budget of one observation; larger trees are returned partially
/// with a warning.
const SNAPSHOT_BUDGET: Duration = Duration::from_millis(500);
/// Hard bound on one observation: the wait for a busy app, the budget, and
/// a margin for calls that do not take the budget into account.
const SNAPSHOT_BACKSTOP: Duration = Duration::from_millis(2000);
/// First call to an application in an observation, when the app is
/// confirmed blocked in a nested main loop inside an earlier accessibility
/// action: it will not answer. An app that does not answer within its probe
/// is treated as unresponsive and skipped for a while.
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// The same first call for an app that is merely busy (populating the file
/// chooser it just opened takes 300-500 ms): the observation waits for it,
/// because an observation without a tree costs the model a turn.
const BUSY_WAIT: Duration = Duration::from_millis(1000);
const UNRESPONSIVE_RETRY: Duration = Duration::from_secs(3);
/// Probe of an app's accessibility connection before key input.
const KEY_PROBE_TIMEOUT: Duration = Duration::from_millis(150);
/// How long key input waits for a busy (not confirmed blocked) app.
const KEY_BUSY_WAIT: Duration = Duration::from_millis(1000);
/// Re-probe of an app marked unresponsive.
const REPROBE_TIMEOUT: Duration = Duration::from_millis(60);
const CACHE_MAX_AGE: Duration = Duration::from_secs(5);
/// After a bulk cache fetch timed out, live traversal for this long.
const CACHE_RETRY: Duration = Duration::from_secs(10);
/// Part of the observation budget kept for geometry and element details.
const RENDER_RESERVE: Duration = Duration::from_millis(150);
const TOGGLE_VERIFY: Duration = Duration::from_millis(1000);
const MAX_TREE_NODES: usize = 4000;
const MAX_LIVE_NODES: usize = 1500;
const MAX_DEPTH: usize = 80;
const MAX_CANDIDATES: usize = 2400;
const MAX_LINES: usize = 1200;
const MAX_TEXT_CHARS: i32 = 240;
const MAX_NAME_CHARS: usize = 160;
const MAX_CHILDREN_LIVE: i32 = 1000;
/// Children shown of a container larger than `MAX_CHILDREN_LIVE`.
const MAX_VISIBLE_KIDS: i32 = 150;
const MAX_TOPLEVELS: usize = 64;
const MAX_ACTIONS_SHOWN: usize = 8;
const MAX_INSERT_BYTES: usize = 1 << 20;
const CONCURRENCY: usize = 24;

// ---------------------------------------------------------------------------
// AT-SPI names and constants
// ---------------------------------------------------------------------------

const NULL_PATH: &str = "/org/a11y/atspi/null";
const REGISTRY_BUS: &str = "org.a11y.atspi.Registry";
const REGISTRY_ROOT: &str = "/org/a11y/atspi/accessible/root";
const REGISTRY_PATH: &str = "/org/a11y/atspi/registry";
const CACHE_PATH: &str = "/org/a11y/atspi/cache";
const PROPS: &str = "org.freedesktop.DBus.Properties";
const I_ACC: &str = "org.a11y.atspi.Accessible";
const I_ACTION: &str = "org.a11y.atspi.Action";
const I_COMPONENT: &str = "org.a11y.atspi.Component";
const I_EDIT: &str = "org.a11y.atspi.EditableText";
const I_TEXT: &str = "org.a11y.atspi.Text";
const I_VALUE: &str = "org.a11y.atspi.Value";
const I_SELECTION: &str = "org.a11y.atspi.Selection";
const I_TABLE: &str = "org.a11y.atspi.Table";
const I_CACHE: &str = "org.a11y.atspi.Cache";
const I_REGISTRY: &str = "org.a11y.atspi.Registry";

const COORD_SCREEN: u32 = 0;
const SCROLL_ANYWHERE: u32 = 6;

// AtspiStateType bit numbers.
const S_ACTIVE: u32 = 1;
const S_BUSY: u32 = 3;
const S_CHECKED: u32 = 4;
const S_COLLAPSED: u32 = 5;
const S_DEFUNCT: u32 = 6;
const S_EDITABLE: u32 = 7;
const S_ENABLED: u32 = 8;
const S_EXPANDABLE: u32 = 9;
const S_EXPANDED: u32 = 10;
const S_FOCUSABLE: u32 = 11;
const S_FOCUSED: u32 = 12;
const S_MODAL: u32 = 16;
const S_PRESSED: u32 = 20;
const S_SELECTABLE: u32 = 22;
const S_SELECTED: u32 = 23;
const S_SENSITIVE: u32 = 24;
const S_SHOWING: u32 = 25;
const S_SINGLE_LINE: u32 = 26;
const S_INDETERMINATE: u32 = 32;
const S_REQUIRED: u32 = 33;
const S_INVALID_ENTRY: u32 = 36;
const S_CHECKABLE: u32 = 41;
const S_READ_ONLY: u32 = 43;

// AtspiRole values.
const R_ALERT: u32 = 2;
const R_CHECK_BOX: u32 = 7;
const R_CHECK_MENU_ITEM: u32 = 8;
const R_DIALOG: u32 = 16;
const R_FILE_CHOOSER: u32 = 19;
const R_FILLER: u32 = 20;
const R_FRAME: u32 = 23;
const R_GLASS_PANE: u32 = 24;
const R_LAYERED_PANE: u32 = 30;
const R_LIST: u32 = 31;
const R_MENU: u32 = 33;
const R_MENU_ITEM: u32 = 35;
const R_PANEL: u32 = 39;
const R_PASSWORD_TEXT: u32 = 40;
const R_POPUP_MENU: u32 = 41;
const R_RADIO_BUTTON: u32 = 44;
const R_RADIO_MENU_ITEM: u32 = 45;
const R_ROOT_PANE: u32 = 46;
const R_SCROLL_PANE: u32 = 49;
const R_TOGGLE_BUTTON: u32 = 62;
const R_TOOL_TIP: u32 = 64;
const R_UNKNOWN: u32 = 67;
const R_VIEWPORT: u32 = 68;
const R_WINDOW: u32 = 69;
const R_APPLICATION: u32 = 75;
const R_REDUNDANT_OBJECT: u32 = 86;
const R_LIST_BOX: u32 = 98;
const R_SWITCH: u32 = 130;
const R_ENTRY: u32 = 79;
const R_SPIN_BUTTON: u32 = 52;
const R_EDITBAR: u32 = 77;
const R_DIRECTORY_PANE: u32 = 17;
const R_ICON: u32 = 26;
const R_LIST_ITEM: u32 = 32;
const R_TABLE_CELL: u32 = 56;
const R_TREE_ITEM: u32 = 91;
const R_MENU_BAR: u32 = 34;
const R_PUSH_BUTTON: u32 = 43;

const A11Y_DISABLED: &str = "org.a11y.Status.IsEnabled is false on the session bus; Qt and Electron applications may need QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 or --force-renderer-accessibility to expose a tree";

const ROLE_NAMES: [&str; 131] = [
    "invalid",
    "accelerator label",
    "alert",
    "animation",
    "arrow",
    "calendar",
    "canvas",
    "check box",
    "check menu item",
    "color chooser",
    "column header",
    "combo box",
    "date editor",
    "desktop icon",
    "desktop frame",
    "dial",
    "dialog",
    "directory pane",
    "drawing area",
    "file chooser",
    "filler",
    "focus traversable",
    "font chooser",
    "frame",
    "glass pane",
    "html container",
    "icon",
    "image",
    "internal frame",
    "label",
    "layered pane",
    "list",
    "list item",
    "menu",
    "menu bar",
    "menu item",
    "option pane",
    "page tab",
    "page tab list",
    "panel",
    "password text",
    "popup menu",
    "progress bar",
    "button",
    "radio button",
    "radio menu item",
    "root pane",
    "row header",
    "scroll bar",
    "scroll pane",
    "separator",
    "slider",
    "spin button",
    "split pane",
    "status bar",
    "table",
    "table cell",
    "table column header",
    "table row header",
    "tearoff menu item",
    "terminal",
    "text",
    "toggle button",
    "tool bar",
    "tool tip",
    "tree",
    "tree table",
    "unknown",
    "viewport",
    "window",
    "extended",
    "header",
    "footer",
    "paragraph",
    "ruler",
    "application",
    "autocomplete",
    "editbar",
    "embedded",
    "entry",
    "chart",
    "caption",
    "document frame",
    "heading",
    "page",
    "section",
    "redundant object",
    "form",
    "link",
    "input method window",
    "table row",
    "tree item",
    "document spreadsheet",
    "document presentation",
    "document text",
    "document web",
    "document email",
    "comment",
    "list box",
    "grouping",
    "image map",
    "notification",
    "info bar",
    "level bar",
    "title bar",
    "block quote",
    "audio",
    "video",
    "definition",
    "article",
    "landmark",
    "log",
    "marquee",
    "math",
    "rating",
    "timer",
    "static",
    "math fraction",
    "math root",
    "subscript",
    "superscript",
    "description list",
    "description term",
    "description value",
    "footnote",
    "content deletion",
    "content insertion",
    "mark",
    "suggestion",
    "push button menu",
    "switch",
];

fn role_name(role: u32) -> String {
    ROLE_NAMES
        .get(role as usize)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("role#{role}"))
}

fn has(states: u64, bit: u32) -> bool {
    bit < 64 && (states >> bit) & 1 == 1
}

fn states_from_wire(words: &[u32]) -> u64 {
    let lo = words.first().copied().unwrap_or(0) as u64;
    let hi = words.get(1).copied().unwrap_or(0) as u64;
    lo | (hi << 32)
}

fn is_toplevel_role(role: u32) -> bool {
    matches!(
        role,
        R_FRAME | R_DIALOG | R_WINDOW | R_ALERT | R_FILE_CHOOSER
    )
}

fn is_structural_role(role: u32) -> bool {
    matches!(
        role,
        0 | R_FILLER
            | R_PANEL
            | R_UNKNOWN
            | R_VIEWPORT
            | R_SCROLL_PANE
            | R_LAYERED_PANE
            | R_GLASS_PANE
            | R_ROOT_PANE
            | R_REDUNDANT_OBJECT
    )
}

fn is_popup_role(role: u32) -> bool {
    matches!(role, R_MENU | R_POPUP_MENU | R_TOOL_TIP | R_WINDOW)
}

fn is_toggle_role(role: u32) -> bool {
    matches!(
        role,
        R_CHECK_BOX
            | R_CHECK_MENU_ITEM
            | R_TOGGLE_BUTTON
            | R_SWITCH
            | R_RADIO_BUTTON
            | R_RADIO_MENU_ITEM
    )
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Accessibility observation of one window.
#[derive(Debug, Clone, Default)]
pub struct AxSnapshot {
    /// Pruned, indented element tree. Each element line starts with `[id]`.
    pub text: String,
    /// Explicit degradations and failures. Empty `text` always has a warning.
    pub warnings: Vec<String>,
}

/// Outcome of a semantic text input (`type_text`, `paste`).
pub enum ClickPlan {
    /// Pointer click at a screen point. `popup`: the element's screen
    /// rectangle, which must lie on the app's popup window under the point
    /// (an open menu); otherwise no popup may cover the point.
    Pointer {
        x: i32,
        y: i32,
        popup: Option<(i32, i32, i32, i32)>,
    },
    /// AT-SPI action (no visible geometry, or the centre is covered).
    Action { warning: Option<String> },
}

pub enum TextInput {
    /// Inserted through EditableText; warnings such as a verification mismatch.
    Inserted(Vec<String>),
    /// No accessible editable text has focus (the reason); the caller may
    /// fall back to key events.
    NoTarget(String),
    /// The target is a single-line field and the text has newlines: the
    /// caller inserts the lines separately and presses Return between them.
    SingleLine,
}

pub struct Accessibility {
    conn: Connection,
    events: Arc<Mutex<EventLog>>,
    listener: Option<tokio::task::JoinHandle<()>>,
    events_registered: bool,
    /// Reported with the first observation of the session only.
    setup_warnings: Vec<String>,
    /// org.a11y.Status.IsEnabled was false on the session bus.
    a11y_disabled: bool,
    pids: HashMap<String, u32>,
    /// Application buses that recently failed to answer, until when they
    /// are skipped.
    unresponsive: HashMap<String, Instant>,
    /// Buses where an action call timed out: the app is inside that call
    /// (a nested main loop) until it answers a probe again.
    blocked: std::collections::HashSet<String>,
    /// Warnings of the last semantic action (reported, not errors).
    pub action_warnings: Vec<String>,
    caches: HashMap<String, AppCache>,
    obs: Observations,
    /// Event sequence when the last action started (`begin_action`).
    last_action_seq: u64,
}

impl Drop for Accessibility {
    fn drop(&mut self) {
        if let Some(h) = self.listener.take() {
            h.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

type WireRef = (String, OwnedObjectPath);
type NewItem = (
    WireRef,
    WireRef,
    WireRef,
    i32,
    i32,
    Vec<String>,
    String,
    u32,
    String,
    Vec<u32>,
);
type OldItem = (
    WireRef,
    WireRef,
    WireRef,
    Vec<WireRef>,
    Vec<String>,
    String,
    u32,
    String,
    Vec<u32>,
);

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ObjRef {
    bus: String,
    path: String,
}

impl ObjRef {
    fn new(bus: &str, path: &str) -> Self {
        ObjRef {
            bus: bus.to_string(),
            path: path.to_string(),
        }
    }
    fn from_wire(w: &WireRef) -> Self {
        ObjRef {
            bus: w.0.clone(),
            path: w.1.as_str().to_string(),
        }
    }
    fn is_null(&self) -> bool {
        self.bus.is_empty() || self.path == NULL_PATH || self.path.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
struct Node {
    parent: Option<ObjRef>,
    child_count: i32,
    ifaces: Vec<String>,
    name: String,
    role: u32,
    desc: String,
    states: u64,
}

impl Node {
    fn has_iface(&self, iface: &str) -> bool {
        self.ifaces.iter().any(|i| i == iface)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn from_tuple(t: (i32, i32, i32, i32)) -> Option<Rect> {
        // Toolkits report (-1,-1,-1,-1) or i32::MIN for unknown geometry.
        if t.2 < 0 || t.3 < 0 || t.0 == i32::MIN || t.1 == i32::MIN {
            return None;
        }
        Some(Rect {
            x: t.0,
            y: t.1,
            w: t.2,
            h: t.3,
        })
    }
    fn intersect(&self, o: &Rect) -> Option<Rect> {
        let (x0, y0) = (self.x.max(o.x), self.y.max(o.y));
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        (x1 > x0 && y1 > y0).then_some(Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    }
    fn outside(&self, w: i32, h: i32) -> bool {
        self.x >= w
            || self.y >= h
            || self.x.saturating_add(self.w) <= 0
            || self.y.saturating_add(self.h) <= 0
    }
}

#[derive(Default)]
struct EventLog {
    /// Global monotonically increasing event sequence.
    seq: u64,
    /// Sequence of the last event seen from each sender unique name.
    last: HashMap<String, u64>,
    /// Objects reported removed/defunct, with the sequence of the report.
    removed: HashMap<ObjRef, u64>,
    /// Unique names that left the bus, with the sequence.
    gone: HashMap<String, u64>,
    listening: bool,
    /// Oldest snapshot sequence any window state still relies on; entries at
    /// or below it can no longer invalidate anything and may be pruned.
    floor: u64,
    /// Observations older than this sequence lost removal records to the
    /// hard size bound and must be repeated before acting.
    lost_before: u64,
    /// Last object of each application that reported gaining focus.
    focused: HashMap<String, ObjRef>,
    /// Objects named by an event since the daemon's mirror of their
    /// application last looked at them, per application bus, with how much
    /// of the mirror around them the event invalidates.
    dirty: HashMap<String, Vec<(ObjRef, Forget)>>,
    /// Event kinds seen since the last observation (tracing only).
    kinds: HashMap<String, usize>,
}

/// How much of the mirror an event invalidates around the object it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Forget {
    /// The object's own node (a state or name change).
    Node,
    /// Also its child list (a child added or removed).
    Kids,
    /// Also its children's nodes (a tree view's visible-data-changed, row
    /// and column changes: the only notice GTK gives of cell changes).
    Deep,
}

/// What an event invalidates in the mirror, if anything. Text, value and
/// geometry are read live, so their events change nothing cached.
fn forget_for(member: &str, detail: &str) -> Option<Forget> {
    match member {
        "StateChanged" => Some(Forget::Node),
        "PropertyChange" => matches!(
            detail,
            "accessible-name" | "accessible-description" | "accessible-parent" | "accessible-role"
        )
        .then_some(Forget::Node),
        "ChildrenChanged" => Some(Forget::Kids),
        "VisibleDataChanged" | "RowInserted" | "RowDeleted" | "RowReordered" | "ColumnInserted"
        | "ColumnDeleted" | "ColumnReordered" | "ModelChanged" => Some(Forget::Deep),
        _ => None,
    }
}

const LOG_PRUNE_AT: usize = 20_000;
const LOG_HARD_MAX: usize = 200_000;

impl EventLog {
    fn bump(&mut self, sender: &str) -> u64 {
        self.seq += 1;
        self.last.insert(sender.to_string(), self.seq);
        self.seq
    }

    /// Bounds the removal records by dropping those no snapshot depends on;
    /// only if that is not enough, drop everything and invalidate older
    /// observations instead of silently forgetting removals.
    fn prune(&mut self) {
        if self.removed.len() + self.gone.len() <= LOG_PRUNE_AT {
            return;
        }
        let floor = self.floor;
        self.removed.retain(|_, s| *s > floor);
        self.gone.retain(|_, s| *s > floor);
        if self.removed.len() + self.gone.len() > LOG_HARD_MAX {
            self.removed.clear();
            self.gone.clear();
            self.lost_before = self.seq + 1;
        }
    }
}

/// The daemon's mirror of one application's objects: the bulk
/// `Cache.GetItems` result plus every object and child list fetched live
/// since. Objects an event names are dropped (with their children) and
/// fetched again when next needed; the whole mirror is rebuilt after
/// `CACHE_MAX_AGE`, as a backstop for changes no event announced.
struct AppCache {
    fetched_at: Instant,
    /// False when the application does not implement the Cache interface,
    /// or its last bulk fetch failed (then until `retry_at`).
    supported: bool,
    retry_at: Option<Instant>,
    nodes: HashMap<ObjRef, Node>,
    /// Complete child lists (never the visible slice of a huge container).
    kids: HashMap<ObjRef, Vec<ObjRef>>,
    /// Action names, which a widget keeps for its lifetime.
    actions: HashMap<ObjRef, Vec<String>>,
}

impl AppCache {
    fn empty(supported: bool, retry_at: Option<Instant>) -> Self {
        AppCache {
            fetched_at: Instant::now(),
            supported,
            retry_at,
            nodes: HashMap::new(),
            kids: HashMap::new(),
            actions: HashMap::new(),
        }
    }

    /// Forgets an object an event named, and around it what the event
    /// invalidates (`Forget`).
    fn forget(&mut self, r: &ObjRef, how: Forget) {
        self.nodes.remove(r);
        self.actions.remove(r);
        if how == Forget::Node {
            return;
        }
        if let Some(kids) = self.kids.remove(r) {
            if how == Forget::Deep {
                for k in kids {
                    self.nodes.remove(&k);
                    self.actions.remove(&k);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct Elem {
    r: ObjRef,
    role: u32,
    name: String,
    ifaces: Vec<String>,
    states: u64,
}

impl Elem {
    fn has_iface(&self, iface: &str) -> bool {
        self.ifaces.iter().any(|i| i == iface)
    }
}

struct WindowState {
    pid: u32,
    frame: Option<ObjRef>,
    ids: HashMap<ObjRef, (u32, u32)>,
    elements: HashMap<u32, Elem>,
    valid: bool,
    snap_seq: u64,
}

impl WindowState {
    fn new(pid: u32) -> Self {
        WindowState {
            pid,
            frame: None,
            ids: HashMap::new(),
            elements: HashMap::new(),
            valid: false,
            snap_seq: 0,
        }
    }
    fn id_for(&mut self, r: &ObjRef, role: u32, next_id: &mut u32) -> u32 {
        if let Some(&(id, old_role)) = self.ids.get(r) {
            if old_role == role {
                return id;
            }
        }
        *next_id += 1;
        self.ids.insert(r.clone(), (*next_id, role));
        *next_id
    }
}

/// Per-window observation state plus the session-wide element ID counter.
/// Window states are replaced on owner change, frame remap and turn reset;
/// the counter never is, so IDs are unique for the daemon's lifetime.
#[derive(Default)]
struct Observations {
    last_id: u32,
    windows: HashMap<u32, WindowState>,
}

impl Observations {
    /// State for a new snapshot of `w`, invalidated until the snapshot
    /// completes; a window that changed owner starts over.
    fn begin(&mut self, w: &WindowInfo) -> &mut WindowState {
        if self
            .windows
            .get(&w.window_id)
            .is_some_and(|ws| ws.pid != w.pid)
        {
            self.windows.remove(&w.window_id);
        }
        let ws = self
            .windows
            .entry(w.window_id)
            .or_insert_with(|| WindowState::new(w.pid));
        // The previous element map stays usable until a new snapshot
        // succeeds (`snapshot_inner` replaces it at the end).
        ws
    }

    /// Binds the window to its accessible frame. Returns true when the frame
    /// changed, which discards the window's earlier IDs and diff baseline.
    fn bind_frame(&mut self, window_id: u32, frame: &ObjRef) -> bool {
        let Some(ws) = self.windows.get_mut(&window_id) else {
            return false;
        };
        let changed = ws.frame.as_ref().is_some_and(|f| f != frame);
        if changed {
            *ws = WindowState::new(ws.pid);
        }
        ws.frame = Some(frame.clone());
        changed
    }

    fn id_for(&mut self, window_id: u32, r: &ObjRef, role: u32) -> u32 {
        let ws = self
            .windows
            .get_mut(&window_id)
            .expect("begin() precedes id assignment");
        ws.id_for(r, role, &mut self.last_id)
    }

    fn reset(&mut self) {
        self.windows.clear();
    }

    /// The window whose observations issued element `idx` (whether or not
    /// its latest observation still shows it).
    /// Accessibility bus of element `idx` of a window.
    fn bus_of(&self, window_id: u32, idx: u32) -> Option<String> {
        self.windows
            .get(&window_id)
            .and_then(|ws| ws.elements.get(&idx))
            .map(|e| e.r.bus.clone())
    }

    fn window_of(&self, idx: u32) -> Option<u32> {
        self.windows
            .iter()
            .find(|(_, ws)| ws.elements.contains_key(&idx))
            .or_else(|| {
                self.windows
                    .iter()
                    .find(|(_, ws)| ws.ids.values().any(|&(id, _)| id == idx))
            })
            .map(|(&w, _)| w)
    }

    /// Oldest snapshot sequence a still-usable observation depends on.
    fn floor(&self) -> u64 {
        self.windows
            .values()
            .filter(|ws| ws.valid)
            .map(|ws| ws.snap_seq)
            .min()
            .unwrap_or(u64::MAX)
    }
}

#[derive(Default)]
struct Tree {
    nodes: HashMap<ObjRef, Node>,
    kids: HashMap<ObjRef, Vec<ObjRef>>,
    /// Containers whose `kids` are only the visible slice.
    partial: std::collections::HashSet<ObjRef>,
    live_nodes: usize,
    cached_nodes: usize,
    truncated: Vec<String>,
    failures: usize,
}

#[derive(Default, Clone)]
struct Details {
    text: Option<(String, i32)>,
    value: Option<String>,
    /// Action names (None when the object has no Action interface or the
    /// call failed).
    actions: Option<Vec<String>>,
}

struct Candidate {
    r: ObjRef,
    node: Node,
    rect: Option<Rect>,
    score: i32,
    why: Vec<&'static str>,
}

// ---------------------------------------------------------------------------
// D-Bus helpers
// ---------------------------------------------------------------------------

async fn call_raw<B>(
    conn: &Connection,
    r: &ObjRef,
    iface: &str,
    member: &str,
    body: &B,
    t: Duration,
) -> Result<Message>
where
    B: serde::Serialize + DynamicType,
{
    let fut = conn.call_method(
        Some(r.bus.as_str()),
        r.path.as_str(),
        Some(iface),
        member,
        body,
    );
    match tokio::time::timeout(t, fut).await {
        Ok(Ok(m)) => Ok(m),
        Ok(Err(e)) => Err(anyhow!(e).context(format!("{iface}.{member} on {}{}", r.bus, r.path))),
        Err(_) => bail!(
            "{iface}.{member} on {}{} timed out after {} ms",
            r.bus,
            r.path,
            t.as_millis()
        ),
    }
}

async fn call<B, R>(conn: &Connection, r: &ObjRef, iface: &str, member: &str, body: &B) -> Result<R>
where
    B: serde::Serialize + DynamicType,
    R: serde::de::DeserializeOwned + zbus::zvariant::Type,
{
    call_t(conn, r, iface, member, body, CALL_TIMEOUT).await
}

async fn call_t<B, R>(
    conn: &Connection,
    r: &ObjRef,
    iface: &str,
    member: &str,
    body: &B,
    t: Duration,
) -> Result<R>
where
    B: serde::Serialize + DynamicType,
    R: serde::de::DeserializeOwned + zbus::zvariant::Type,
{
    let msg = call_raw(conn, r, iface, member, body, t).await?;
    let v = msg
        .body()
        .deserialize::<R>()
        .with_context(|| format!("unexpected reply type from {iface}.{member}"))?;
    Ok(v)
}

async fn get_prop<R>(conn: &Connection, r: &ObjRef, iface: &str, prop: &str) -> Result<R>
where
    R: TryFrom<OwnedValue>,
    <R as TryFrom<OwnedValue>>::Error: std::fmt::Display,
{
    let v: OwnedValue = call(conn, r, PROPS, "Get", &(iface, prop)).await?;
    R::try_from(v).map_err(|e| anyhow!("property {iface}.{prop}: {e}"))
}

fn pv<T>(m: &HashMap<String, OwnedValue>, k: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    let v = m.get(k)?.try_clone().ok()?;
    T::try_from(v).ok()
}

async fn fetch_node_live(conn: &Connection, r: &ObjRef) -> Result<Node> {
    let (props, role, state, ifaces) = tokio::try_join!(
        call::<_, HashMap<String, OwnedValue>>(conn, r, PROPS, "GetAll", &(I_ACC,)),
        call::<_, u32>(conn, r, I_ACC, "GetRole", &()),
        call::<_, Vec<u32>>(conn, r, I_ACC, "GetState", &()),
        call::<_, Vec<String>>(conn, r, I_ACC, "GetInterfaces", &()),
    )?;
    let parent = pv::<(String, OwnedObjectPath)>(&props, "Parent")
        .map(|w| ObjRef::from_wire(&w))
        .filter(|p| !p.is_null());
    Ok(Node {
        parent,
        child_count: pv::<i32>(&props, "ChildCount").unwrap_or(-1),
        ifaces,
        name: pv::<String>(&props, "Name").unwrap_or_default(),
        role,
        desc: pv::<String>(&props, "Description").unwrap_or_default(),
        states: states_from_wire(&state),
    })
}

async fn live_states(conn: &Connection, r: &ObjRef) -> Result<u64> {
    let s: Vec<u32> = call(conn, r, I_ACC, "GetState", &()).await?;
    Ok(states_from_wire(&s))
}

async fn live_children(conn: &Connection, r: &ObjRef) -> Result<Vec<ObjRef>> {
    let kids: Vec<WireRef> = call(conn, r, I_ACC, "GetChildren", &()).await?;
    Ok(kids
        .iter()
        .map(ObjRef::from_wire)
        .filter(|k| !k.is_null())
        .collect())
}

/// Children of a container too large to traverse (web lists, big tables)
/// that intersect `view` (screen coordinates), at most `MAX_VISIBLE_KIDS`,
/// found by binary search over child indices; assumes children are laid out
/// top to bottom. Without geometry, the first `MAX_VISIBLE_KIDS`.
async fn visible_children(
    conn: &Connection,
    r: &ObjRef,
    count: i32,
    view: Option<Rect>,
) -> Result<(i32, Vec<ObjRef>)> {
    let child = |i: i32| async move {
        let w: WireRef = call(conn, r, I_ACC, "GetChildAtIndex", &(i,)).await?;
        anyhow::Ok(ObjRef::from_wire(&w))
    };
    let bottom = |c: ObjRef| async move {
        extents(conn, &c, COORD_SCREEN)
            .await
            .ok()
            .flatten()
            .map(|e| (e.y, e.y + e.h))
    };
    let mut first = 0;
    if let Some(v) = view {
        let (mut lo, mut hi) = (0, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match bottom(child(mid).await?).await {
                Some((_, b)) if b <= v.y => lo = mid + 1,
                Some(_) => hi = mid,
                None => {
                    lo = 0;
                    break;
                }
            }
        }
        first = lo.min(count.saturating_sub(1)).max(0);
    }
    let mut out = Vec::new();
    for i in first..count.min(first + MAX_VISIBLE_KIDS) {
        let c = child(i).await?;
        if let Some(v) = view {
            if matches!(bottom(c.clone()).await, Some((top, _)) if top >= v.y + v.h) {
                break;
            }
        }
        if !c.is_null() {
            out.push(c);
        }
    }
    Ok((first, out))
}

async fn extents(conn: &Connection, r: &ObjRef, coord: u32) -> Result<Option<Rect>> {
    let t: (i32, i32, i32, i32) = call(conn, r, I_COMPONENT, "GetExtents", &(coord,)).await?;
    Ok(Rect::from_tuple(t))
}

fn is_unsupported(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("UnknownMethod") || s.contains("UnknownObject") || s.contains("UnknownInterface")
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().nth(max).is_some() {
        out.push('…');
    }
    out
}

fn quote(s: &str, max: usize) -> String {
    serde_json::to_string(&truncate_chars(s, max)).unwrap_or_else(|_| "\"?\"".into())
}

fn norm_title(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.4}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

/// Refuse implicit or owner-default buses; daemons are bound to a private
/// bus. Every `;`-separated address must be `unix:path=...` whose
/// percent-decoded, canonicalized path is not the owner's per-user session
/// or accessibility bus under `/run/user/<uid>/`. Abstract sockets have no
/// filesystem identity to check and are refused unless `allow_abstract`.
fn check_private_address(addr: &str, what: &str, allow_abstract: bool) -> Result<()> {
    let a = addr.trim();
    if a.is_empty() {
        bail!("{what} is empty");
    }
    for part in a.split(';').filter(|p| !p.is_empty()) {
        let (transport, rest) = part
            .split_once(':')
            .ok_or_else(|| anyhow!("{what}: malformed D-Bus address {part:?}"))?;
        if transport != "unix" {
            bail!("{what} uses the {transport:?} transport; an explicit unix:path= private bus is required");
        }
        let mut path = None;
        let mut is_abstract = false;
        for kv in rest.split(',').filter(|kv| !kv.is_empty()) {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| anyhow!("{what}: malformed D-Bus address entry {kv:?}"))?;
            let v = unescape_address_value(v)
                .ok_or_else(|| anyhow!("{what}: malformed escape in {kv:?}"))?;
            match k {
                "path" if path.is_none() => path = Some(v),
                "abstract" => is_abstract = true,
                "guid" => {}
                _ => bail!("{what}: unsupported unix address key {k:?} (runtime=, dir= and tmpdir= select implicit locations)"),
            }
        }
        match (path, is_abstract) {
            (Some(p), false) => {
                let c = canonical_path(&p)
                    .with_context(|| format!("{what}: cannot interpret bus path {p:?}"))?;
                if is_owner_default_bus(&c) {
                    bail!(
                        "{what} {a:?} is the owner's default per-user bus ({}); refusing (use a private bus for the daemon session)",
                        c.display()
                    );
                }
            }
            (None, true) if allow_abstract => {}
            (None, true) => bail!("{what} {a:?} is an abstract socket, whose owner cannot be verified; set CUA_ALLOW_ABSTRACT_BUS=1 to accept it"),
            _ => bail!("{what}: a unix address needs exactly one of path= or abstract="),
        }
    }
    Ok(())
}

/// D-Bus address values escape bytes as `%XX`.
fn unescape_address_value(v: &str) -> Option<String> {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Resolves symlinks, `.`/`..` and repeated separators. A path that does
/// not exist (yet) is normalized lexically and its longest existing prefix
/// is resolved.
fn canonical_path(p: &str) -> Result<std::path::PathBuf> {
    use std::path::{Component, Path, PathBuf};
    let path = Path::new(p);
    if !path.is_absolute() {
        bail!("bus path is not absolute");
    }
    if let Ok(c) = std::fs::canonicalize(path) {
        return Ok(c);
    }
    let mut norm = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::Normal(n) => norm.push(n),
            _ => {}
        }
    }
    let mut tail = Vec::new();
    let mut existing = norm;
    while !existing.exists() {
        match existing.file_name() {
            Some(n) => {
                tail.push(n.to_owned());
                existing.pop();
            }
            None => break,
        }
    }
    let mut out = std::fs::canonicalize(&existing).unwrap_or(existing);
    out.extend(tail.iter().rev());
    Ok(out)
}

/// The owner's defaults: `/run/user/<uid>/bus` (session) and
/// `/run/user/<uid>/at-spi/bus*` (accessibility).
fn is_owner_default_bus(c: &std::path::Path) -> bool {
    let Ok(rest) = c.strip_prefix("/run/user") else {
        return false;
    };
    let mut parts = rest.iter().map(|p| p.to_string_lossy());
    let uid_ok = parts
        .next()
        .is_some_and(|u| !u.is_empty() && u.chars().all(|c| c.is_ascii_digit()));
    let tail: Vec<_> = parts.collect();
    uid_ok
        && match tail.as_slice() {
            [bus] => bus == "bus",
            [dir, bus] => dir == "at-spi" && bus.starts_with("bus"),
            _ => false,
        }
}

fn allow_abstract_bus() -> bool {
    std::env::var("CUA_ALLOW_ABSTRACT_BUS").is_ok_and(|v| v == "1")
}

async fn resolve_bus_address(warnings: &mut Vec<String>) -> Result<String> {
    if let Ok(a) = std::env::var("AT_SPI_BUS_ADDRESS") {
        if !a.trim().is_empty() {
            check_private_address(&a, "AT_SPI_BUS_ADDRESS", allow_abstract_bus())?;
            return Ok(a);
        }
    }
    let session = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default();
    if session.trim().is_empty() {
        bail!("neither AT_SPI_BUS_ADDRESS nor an explicit DBUS_SESSION_BUS_ADDRESS is set; refusing implicit bus discovery");
    }
    check_private_address(&session, "DBUS_SESSION_BUS_ADDRESS", allow_abstract_bus())?;
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, async {
        zbus::connection::Builder::address(session.as_str())?
            .method_timeout(CALL_TIMEOUT * 2)
            .build()
            .await
    })
    .await
    .map_err(|_| anyhow!("connecting to the session bus timed out"))?
    .context("connecting to the explicit session bus")?;
    let bus = ObjRef::new("org.a11y.Bus", "/org/a11y/bus");
    let addr: String = call(&conn, &bus, "org.a11y.Bus", "GetAddress", &())
        .await
        .context("org.a11y.Bus.GetAddress on the session bus (is at-spi-bus-launcher running?)")?;
    check_private_address(
        &addr,
        "accessibility bus address from org.a11y.Bus",
        allow_abstract_bus(),
    )?;
    match get_prop::<bool>(&conn, &bus, "org.a11y.Status", "IsEnabled").await {
        Ok(true) => {}
        Ok(false) => warnings.push(A11Y_DISABLED.into()),
        Err(e) => warnings.push(format!("could not read org.a11y.Status.IsEnabled: {e:#}")),
    }
    Ok(addr)
}

fn handle_event(log: &Mutex<EventLog>, msg: &Message) {
    let header = msg.header();
    let Some(member) = header.member().map(|m| m.as_str().to_string()) else {
        return;
    };
    let iface = header
        .interface()
        .map(|i| i.as_str().to_string())
        .unwrap_or_default();
    let sender = header.sender().map(|s| s.as_str().to_string());
    let path = header
        .path()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let mut log = match log.lock() {
        Ok(l) => l,
        Err(p) => p.into_inner(),
    };
    if iface == "org.freedesktop.DBus" && member == "NameOwnerChanged" {
        if let Ok((name, _old, new)) = msg.body().deserialize::<(String, String, String)>() {
            if new.is_empty() && name.starts_with(':') {
                let s = log.bump(&name);
                log.gone.insert(name, s);
            }
        }
        return;
    }
    let Some(sender) = sender else { return };
    let seq = log.bump(&sender);
    let body = msg.body();
    if iface == I_CACHE {
        // AddAccessible carries the new object (and its parent, whose child
        // list grew); RemoveAccessible the object that went.
        let (obj, parent) = if let Ok(it) = body.deserialize::<NewItem>() {
            (ObjRef::from_wire(&it.0), Some(ObjRef::from_wire(&it.2)))
        } else if let Ok(it) = body.deserialize::<OldItem>() {
            (ObjRef::from_wire(&it.0), Some(ObjRef::from_wire(&it.2)))
        } else if let Ok(w) = body.deserialize::<WireRef>() {
            (ObjRef::from_wire(&w), None)
        } else {
            return;
        };
        if crate::trace_on() {
            *log.kinds.entry(member.clone()).or_default() += 1;
        }
        let dirty = log.dirty.entry(sender.clone()).or_default();
        dirty.push((obj.clone(), Forget::Kids));
        dirty.extend(parent.filter(|p| !p.is_null()).map(|p| (p, Forget::Kids)));
        if member == "RemoveAccessible" {
            log.removed.insert(obj, seq);
        }
        log.prune();
        return;
    }
    // Object events carry (detail, d1, d2, any_data[, properties]).
    let parsed = body
        .deserialize::<(String, i32, i32, OwnedValue, HashMap<String, OwnedValue>)>()
        .map(|(d, a, b, v, _)| (d, a, b, v))
        .or_else(|_| body.deserialize::<(String, i32, i32, OwnedValue)>())
        .ok();
    let detail = parsed.as_ref().map(|p| p.0.as_str()).unwrap_or_default();
    if crate::trace_on() {
        *log.kinds.entry(format!("{member}:{detail}")).or_default() += 1;
        if log.kinds.len() == 1
            || (member == "ChildrenChanged" && path == REGISTRY_ROOT)
            || iface.ends_with("Window")
        {
            eprintln!(
                "trace atspi {member}:{detail} {sender}{path} t={:?}",
                crate::since_start()
            );
        }
    }
    if let Some(how) = forget_for(&member, detail) {
        let dirty = log.dirty.entry(sender.clone()).or_default();
        dirty.push((ObjRef::new(&sender, &path), how));
        if dirty.len() > LOG_HARD_MAX {
            // Too many to track: the next observation drops the whole mirror.
            dirty.clear();
            dirty.push((ObjRef::new(&sender, "*"), Forget::Deep));
        }
    }
    match member.as_str() {
        "StateChanged" | "ChildrenChanged" => {
            if let Some((detail, d1, _d2, v)) = parsed {
                if member == "StateChanged" && detail == "defunct" && d1 == 1 {
                    log.removed.insert(ObjRef::new(&sender, &path), seq);
                } else if member == "StateChanged" && detail == "focused" {
                    let r = ObjRef::new(&sender, &path);
                    if d1 == 1 {
                        log.focused.insert(sender.clone(), r);
                    } else if log.focused.get(&sender) == Some(&r) {
                        log.focused.remove(&sender);
                    }
                } else if member == "ChildrenChanged" {
                    if let Ok(w) = <(String, OwnedObjectPath)>::try_from(v) {
                        let r = ObjRef::from_wire(&w);
                        let r = if r.bus.is_empty() {
                            ObjRef::new(&sender, &r.path)
                        } else {
                            r
                        };
                        if detail.starts_with("remove") {
                            log.removed.insert(r.clone(), seq);
                        }
                        log.dirty
                            .entry(sender.clone())
                            .or_default()
                            .push((r, Forget::Node));
                    }
                }
            }
        }
        _ => {}
    }
    log.prune();
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl Accessibility {
    /// Connect to the private accessibility bus, subscribe to invalidation
    /// signals and register for application events.
    /// `first_id` is the first element ID this session issues; the host
    /// passes one above every ID an earlier daemon issued, so IDs stay unique
    /// across daemon restarts.
    pub async fn connect(first_id: u32) -> Result<Self> {
        let mut setup_warnings = Vec::new();
        let addr = resolve_bus_address(&mut setup_warnings).await?;
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, async {
            zbus::connection::Builder::address(addr.as_str())?
                .method_timeout(CALL_TIMEOUT * 4)
                .build()
                .await
        })
        .await
        .map_err(|_| anyhow!("connecting to the accessibility bus timed out"))?
        .context("connecting to the accessibility bus")?;

        let events = Arc::new(Mutex::new(EventLog::default()));
        let mut listener = None;
        let mut subscribed = true;
        let mut streams = Vec::new();
        let mut rules = Vec::new();
        for iface in [
            "org.a11y.atspi.Event.Object",
            "org.a11y.atspi.Event.Window",
            "org.a11y.atspi.Event.Document",
            I_CACHE,
        ] {
            rules.push(
                MatchRule::builder()
                    .msg_type(MsgType::Signal)
                    .interface(iface)?
                    .build(),
            );
        }
        rules.push(
            MatchRule::builder()
                .msg_type(MsgType::Signal)
                .sender("org.freedesktop.DBus")?
                .interface("org.freedesktop.DBus")?
                .member("NameOwnerChanged")?
                .build(),
        );
        for rule in rules {
            match tokio::time::timeout(
                CALL_TIMEOUT * 2,
                MessageStream::for_match_rule(rule, &conn, Some(16_384)),
            )
            .await
            {
                Ok(Ok(s)) => streams.push(s),
                Ok(Err(e)) => {
                    subscribed = false;
                    setup_warnings.push(format!("event subscription failed: {e}"));
                }
                Err(_) => {
                    subscribed = false;
                    setup_warnings.push("event subscription timed out".into());
                }
            }
        }
        if !streams.is_empty() {
            {
                let mut l = events.lock().map_err(|_| anyhow!("event log poisoned"))?;
                l.listening = subscribed;
            }
            let log = events.clone();
            let mut merged = stream::select_all(streams);
            listener = Some(tokio::spawn(async move {
                while let Some(item) = merged.next().await {
                    if let Ok(msg) = item {
                        handle_event(&log, &msg);
                    }
                }
                if let Ok(mut l) = log.lock() {
                    l.listening = false;
                }
            }));
        }

        // Ask applications to emit events (toolkits may suppress unregistered ones).
        let registry = ObjRef::new(REGISTRY_BUS, REGISTRY_PATH);
        let mut registered = true;
        for ev in ["object:", "window:", "document:"] {
            let props: Vec<String> = Vec::new();
            let r3 = call_raw(
                &conn,
                &registry,
                I_REGISTRY,
                "RegisterEvent",
                &(ev, props, ""),
                CALL_TIMEOUT,
            )
            .await;
            if r3.is_err() {
                if let Err(e) = call_raw(
                    &conn,
                    &registry,
                    I_REGISTRY,
                    "RegisterEvent",
                    &(ev,),
                    CALL_TIMEOUT,
                )
                .await
                {
                    registered = false;
                    setup_warnings.push(format!("Registry.RegisterEvent({ev}) failed: {e:#}"));
                }
            }
        }
        if !(subscribed && registered) {
            setup_warnings.push(
                "event-based cache invalidation is degraded; the bulk cache is refetched on every observation".into(),
            );
        }

        // Only relevant when an app shows no tree; GTK apps with atk-bridge
        // and Chromium with --force-renderer-accessibility work regardless.
        let a11y_disabled = setup_warnings.iter().any(|w| w == A11Y_DISABLED);
        setup_warnings.retain(|w| w != A11Y_DISABLED);
        Ok(Accessibility {
            a11y_disabled,
            conn,
            events,
            listener,
            events_registered: registered,
            setup_warnings,
            pids: HashMap::new(),
            unresponsive: HashMap::new(),
            blocked: Default::default(),
            action_warnings: Vec::new(),
            caches: HashMap::new(),
            obs: Observations {
                last_id: first_id.saturating_sub(1),
                windows: HashMap::new(),
            },
            last_action_seq: 0,
        })
    }

    /// Highest element ID issued so far in this session.
    pub fn last_element_id(&self) -> u32 {
        self.obs.last_id
    }

    /// Window (among `wins`) holding element `idx`, for app-scoped actions.
    pub fn window_of_element(&self, idx: u32) -> Option<u32> {
        self.obs.window_of(idx)
    }

    /// Whether the window was last observed as an alert (a message or
    /// confirmation dialog without text fields).
    pub fn window_is_alert(&self, w: &WindowInfo) -> bool {
        self.obs.windows.get(&w.window_id).is_some_and(|ws| {
            ws.pid == w.pid
                && ws
                    .frame
                    .as_ref()
                    .and_then(|f| ws.ids.get(f))
                    .is_some_and(|&(_, role)| role == R_ALERT)
        })
    }

    /// Role of the window's accessible frame in its latest usable
    /// observation; None when the window has no mapped frame.
    pub fn window_role(&self, w: &WindowInfo) -> Option<u32> {
        let ws = self
            .obs
            .windows
            .get(&w.window_id)
            .filter(|ws| ws.pid == w.pid && ws.valid)?;
        ws.frame
            .as_ref()
            .and_then(|f| ws.ids.get(f))
            .map(|&(_, role)| role)
    }

    /// The window's only editable text field in its latest observation.
    pub fn sole_text_field(&self, w: &WindowInfo) -> Option<u32> {
        let ws = self
            .obs
            .windows
            .get(&w.window_id)
            .filter(|ws| ws.pid == w.pid && ws.valid)?;
        let mut fields = ws.elements.iter().filter(|(_, e)| {
            e.has_iface(I_EDIT) && has(e.states, S_EDITABLE) && !has(e.states, S_READ_ONLY)
        });
        match (fields.next(), fields.next()) {
            (Some((&i, _)), None) => Some(i),
            _ => None,
        }
    }

    /// Text of the object that last reported focus in the window's app, if
    /// it has the Text interface (readback after typing key events).
    pub async fn focused_text(&self, w: &WindowInfo) -> Option<String> {
        let ws = self
            .obs
            .windows
            .get(&w.window_id)
            .filter(|ws| ws.pid == w.pid)?;
        let bus = ws.frame.as_ref()?.bus.clone();
        let r = self.events.lock().ok()?.focused.get(&bus).cloned()?;
        let n: i32 = get_prop(&self.conn, &r, I_TEXT, "CharacterCount")
            .await
            .ok()?;
        call(&self.conn, &r, I_TEXT, "GetText", &(0i32, n.min(4000)))
            .await
            .ok()
    }

    /// Whether element `idx` was last observed as a push button.
    pub fn element_is_button(&self, idx: u32) -> bool {
        self.obs.windows.values().any(|ws| {
            ws.ids
                .values()
                .any(|&(id, role)| id == idx && role == R_PUSH_BUTTON)
        })
    }

    /// Clear observation state (element bindings, diffs and cached trees).
    /// The element ID counter continues, so earlier IDs never resolve again.
    pub fn reset(&mut self) {
        self.obs.reset();
        self.caches.clear();
        self.publish_floor();
    }

    fn publish_floor(&self) {
        if let Ok(mut l) = self.events.lock() {
            l.floor = self.obs.floor().min(l.seq);
        }
    }

    /// A probe for the settle loop: reports whether the window's application
    /// emitted accessibility events since the previous call (the baseline is
    /// taken now, before the action). Always false until the window has been
    /// mapped to an accessible frame.
    pub fn activity_probe(&self, w: &WindowInfo) -> impl FnMut() -> bool + 'static {
        let bus = self
            .obs
            .windows
            .get(&w.window_id)
            .filter(|ws| ws.pid == w.pid)
            .and_then(|ws| ws.frame.as_ref())
            .map(|f| f.bus.clone());
        let log = self.events.clone();
        let read = move || -> u64 {
            match &bus {
                Some(b) => log
                    .lock()
                    .map(|l| l.last.get(b).copied().unwrap_or(0))
                    .unwrap_or(0),
                None => 0,
            }
        };
        let mut seen = read();
        move || {
            let now = read();
            let changed = now != seen;
            seen = now;
            changed
        }
    }

    fn event_seq(&self) -> u64 {
        self.events.lock().map(|l| l.seq).unwrap_or(u64::MAX)
    }

    fn invalidation_live(&self) -> bool {
        self.events_registered
            && self
                .listener
                .as_ref()
                .map(|h| !h.is_finished())
                .unwrap_or(false)
            && self.events.lock().map(|l| l.listening).unwrap_or(false)
    }

    /// Observe a window. Accessibility failures never fail the observation;
    /// they are returned as warnings with empty `text`.
    pub async fn snapshot(&mut self, window: &WindowInfo) -> Result<AxSnapshot> {
        let mut warnings = std::mem::take(&mut self.setup_warnings);
        // The budget bounds every phase; this is the backstop for calls
        // that do not take it into account.
        let r = tokio::time::timeout(
            SNAPSHOT_BACKSTOP,
            self.snapshot_inner(window, &mut warnings),
        )
        .await
        .unwrap_or_else(|_| {
            Err(anyhow!(
                "observation exceeded its {} ms budget",
                SNAPSHOT_BUDGET.as_millis()
            ))
        });
        match r {
            Ok(text) => Ok(AxSnapshot { text, warnings }),
            Err(e) => {
                // Earlier element IDs stay valid (they are revalidated
                // before use); only a successful snapshot replaces them.
                warnings.push(format!("accessibility tree unavailable: {e:#}"));
                Ok(AxSnapshot {
                    text: String::new(),
                    warnings,
                })
            }
        }
    }

    async fn registry_apps(&self) -> Result<Vec<ObjRef>> {
        let root = ObjRef::new(REGISTRY_BUS, REGISTRY_ROOT);
        live_children(&self.conn, &root)
            .await
            .context("listing applications from the AT-SPI registry")
    }

    async fn app_pids(&mut self, apps: &[ObjRef]) {
        let unknown: Vec<String> = apps
            .iter()
            .map(|a| a.bus.clone())
            .filter(|b| !self.pids.contains_key(b))
            .collect();
        let conn = self.conn.clone();
        let results: Vec<(String, Option<u32>)> = stream::iter(unknown)
            .map(|bus| {
                let conn = conn.clone();
                async move {
                    let dbus = ObjRef::new("org.freedesktop.DBus", "/org/freedesktop/DBus");
                    let pid = call::<_, u32>(
                        &conn,
                        &dbus,
                        "org.freedesktop.DBus",
                        "GetConnectionUnixProcessID",
                        &(bus.as_str(),),
                    )
                    .await
                    .ok();
                    (bus, pid)
                }
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;
        for (bus, pid) in results {
            if let Some(pid) = pid {
                self.pids.insert(bus, pid);
            }
        }
    }

    /// How long the first call of an observation waits for the app: a busy
    /// app gets `BUSY_WAIT`, one confirmed blocked inside an earlier action
    /// call `PROBE_TIMEOUT`.
    fn probe_timeout(&self, bus: &str) -> Duration {
        if self.blocked.contains(bus) {
            PROBE_TIMEOUT
        } else {
            BUSY_WAIT
        }
    }

    /// The window's accessible frame and its current node. The frame bound
    /// by an earlier observation is reused while it is still showing (one
    /// call, which also serves as the liveness probe); otherwise the app's
    /// top-levels are enumerated and scored.
    async fn map_window(
        &mut self,
        w: &WindowInfo,
        warnings: &mut Vec<String>,
    ) -> Result<(ObjRef, Node)> {
        let bound = self
            .obs
            .windows
            .get(&w.window_id)
            .filter(|ws| ws.pid == w.pid)
            .and_then(|ws| ws.frame.clone());
        if let Some(frame) = bound {
            if self.still_unresponsive(&frame.bus).await {
                bail!(
                    "the app's accessibility interface is not responding (busy{}); pointer input still works",
                    if self.blocked.contains(&frame.bus) {
                        ", and an earlier accessibility action is still running inside it"
                    } else {
                        ""
                    }
                );
            }
            let t = self.probe_timeout(&frame.bus);
            match tokio::time::timeout(t, fetch_node_live(&self.conn, &frame)).await {
                Ok(Ok(node))
                    if has(node.states, S_SHOWING)
                        && !has(node.states, S_DEFUNCT)
                        && node.role != R_APPLICATION =>
                {
                    self.unresponsive.remove(&frame.bus);
                    return Ok((frame, node));
                }
                // Gone or hidden (a dialog closing): map afresh.
                Ok(_) => {}
                Err(_) => {
                    self.unresponsive
                        .insert(frame.bus.clone(), Instant::now() + UNRESPONSIVE_RETRY);
                    bail!(
                        "the app's accessibility interface did not answer within {} ms (busy); pointer input still works",
                        t.as_millis()
                    );
                }
            }
        }
        let apps = self.registry_apps().await?;
        self.app_pids(&apps).await;
        let mine: Vec<ObjRef> = apps
            .into_iter()
            .filter(|a| self.pids.get(&a.bus) == Some(&w.pid))
            .collect();
        if mine.is_empty() {
            bail!(
                "no AT-SPI application is registered for pid {} (app {:?})",
                w.pid,
                w.app_id
            );
        }
        let mut tops: Vec<ObjRef> = Vec::new();
        let mut blocked = 0;
        let mut waited = Duration::ZERO;
        for app in &mine {
            if self.still_unresponsive(&app.bus).await {
                blocked += 1;
                continue;
            }
            let t = self.probe_timeout(&app.bus);
            let kids: Result<Vec<WireRef>> =
                call_t(&self.conn, app, I_ACC, "GetChildren", &(), t).await;
            match kids {
                Ok(k) => {
                    self.unresponsive.remove(&app.bus);
                    tops.extend(
                        k.iter()
                            .map(ObjRef::from_wire)
                            .filter(|k| !k.is_null())
                            .take(MAX_TOPLEVELS),
                    )
                }
                Err(e) if format!("{e:#}").contains("timed out") => {
                    self.unresponsive
                        .insert(app.bus.clone(), Instant::now() + UNRESPONSIVE_RETRY);
                    blocked += 1;
                    waited = waited.max(t);
                }
                Err(e) => warnings.push(format!(
                    "could not list top-level objects of {}: {e:#}",
                    app.bus
                )),
            }
        }
        if tops.is_empty() && blocked > 0 {
            bail!(
                "the app's accessibility interface did not answer within {} ms (busy{}); pointer input still works",
                waited.max(REPROBE_TIMEOUT).as_millis(),
                if mine.iter().any(|a| self.blocked.contains(&a.bus)) {
                    ", and an earlier accessibility action is still running inside it"
                } else {
                    ""
                }
            );
        }
        // The app answered: its top-levels come quickly.
        let deadline = Instant::now() + PROBE_TIMEOUT;
        let conn = &self.conn;
        let fetched: Vec<(ObjRef, Result<Node>, Option<Rect>)> = stream::iter(tops)
            .map(|r| async move {
                let n = fetch_node_live(conn, &r).await;
                let rect = extents(conn, &r, COORD_SCREEN).await.ok().flatten();
                (r, n, rect)
            })
            .buffer_unordered(CONCURRENCY)
            .take_until(tokio::time::sleep_until(deadline.into()))
            .collect()
            .await;
        let title = norm_title(&w.title);
        let (ww, wh) = (w.width as i32, w.height as i32);
        let mut cands: Vec<Candidate> = Vec::new();
        for (r, n, rect) in fetched {
            let Ok(node) = n else { continue };
            if node.role == R_APPLICATION
                || has(node.states, S_DEFUNCT)
                || !has(node.states, S_SHOWING)
            {
                continue;
            }
            let mut score = 0;
            let mut why = Vec::new();
            let name = norm_title(&node.name);
            if !title.is_empty() && name == title {
                score += 4;
                why.push("title");
            } else if !title.is_empty()
                && name.chars().count() >= 3
                && (title.contains(&name) || name.contains(&title))
            {
                score += 2;
                why.push("partial-title");
            }
            if let Some(rc) = rect {
                let (dw, dh) = ((rc.w - ww).abs(), (rc.h - wh).abs());
                if dw <= 2 && dh <= 2 {
                    score += 3;
                    why.push("size");
                } else if dw <= 100 && dh <= 100 {
                    score += 1;
                    why.push("approx-size");
                }
                // The frame is the window plus at most window-manager
                // decorations around it (a title bar, borders): strong
                // evidence for untitled dialogs such as GTK alerts.
                let (ox, oy) = w.origin;
                let (l, t) = (ox - rc.x, oy - rc.y);
                let (r, b) = (rc.x + rc.w - (ox + ww), rc.y + rc.h - (oy + wh));
                if [l, t, r, b].iter().all(|&d| (0..=40).contains(&d)) {
                    score += 2;
                    why.push("position");
                }
            }
            if is_toplevel_role(node.role) {
                why.push("toplevel-role");
            }
            cands.push(Candidate {
                r,
                node,
                rect,
                score,
                why,
            });
        }
        if cands.is_empty() {
            bail!(
                "pid {} has no showing AT-SPI top-level window{}",
                w.pid,
                if self.a11y_disabled {
                    format!(" ({A11Y_DISABLED})")
                } else {
                    String::new()
                }
            );
        }
        cands.sort_by_key(|c| std::cmp::Reverse(c.score));
        let describe = |c: &Candidate| {
            format!(
                "{} {} {}{}",
                role_name(c.node.role),
                quote(&c.node.name, 60),
                c.rect
                    .map(|r| format!("{}x{} ", r.w, r.h))
                    .unwrap_or_default(),
                c.why.join("+")
            )
        };
        let best = &cands[0];
        let tied: Vec<&Candidate> = cands.iter().filter(|c| c.score == best.score).collect();
        if tied.len() > 1 {
            bail!(
                "ambiguous AT-SPI window mapping for window {} {:?}: {} equally plausible frames [{}]",
                w.window_id,
                w.title,
                tied.len(),
                tied.iter().map(|c| describe(c)).collect::<Vec<_>>().join("; ")
            );
        }
        let accept = best.score >= 3 || (cands.len() == 1 && best.score >= 1);
        if !accept {
            bail!(
                "no AT-SPI frame matches window {} {:?} {}x{} by title or size (best: {})",
                w.window_id,
                w.title,
                ww,
                wh,
                describe(best)
            );
        }
        if best.score < 3 {
            warnings.push(format!(
                "weak window mapping (only top-level of pid): {}",
                describe(best)
            ));
        }
        Ok((best.r.clone(), best.node.clone()))
    }

    /// Brings the application's mirror (`AppCache`) up to date for an
    /// observation: objects that events named since the last one are
    /// forgotten; the bulk `Cache.GetItems` is fetched the first time and
    /// again after `CACHE_MAX_AGE`, or on every observation while event
    /// delivery is degraded. Afterwards `self.caches` holds the mirror,
    /// unless the bulk fetch failed outright.
    async fn app_store(&mut self, bus: &str, deadline: Instant, warnings: &mut Vec<String>) {
        let live = self.invalidation_live();
        let dirty = self
            .events
            .lock()
            .ok()
            .and_then(|mut l| l.dirty.remove(bus))
            .unwrap_or_default();
        let reset = !live || dirty.iter().any(|(r, _)| r.path == "*");
        let now = Instant::now();
        match self.caches.get_mut(bus) {
            Some(c) if !reset && now.duration_since(c.fetched_at) < CACHE_MAX_AGE => {
                let before = c.nodes.len();
                for (r, how) in &dirty {
                    c.forget(r, *how);
                }
                if crate::trace_on() {
                    let kinds = self
                        .events
                        .lock()
                        .ok()
                        .map(|mut l| std::mem::take(&mut l.kinds))
                        .unwrap_or_default();
                    let mut kinds: Vec<(String, usize)> = kinds.into_iter().collect();
                    kinds.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
                    let kinds: Vec<String> = kinds
                        .iter()
                        .take(8)
                        .map(|(k, n)| format!("{k}={n}"))
                        .collect();
                    eprintln!(
                        "trace store {bus} dirty={} forgot={} nodes={} kids={} events[{}]",
                        dirty.len(),
                        before - c.nodes.len(),
                        c.nodes.len(),
                        c.kids.len(),
                        kinds.join(" ")
                    );
                }
                return;
            }
            // Without a usable bulk cache, the mirror holds live-fetched
            // objects only: the backstop starts it over.
            Some(c) if !c.supported && c.retry_at.is_none_or(|t| now < t) => {
                c.nodes.clear();
                c.kids.clear();
                c.fetched_at = now;
                return;
            }
            _ => {}
        }
        let cache_ref = ObjRef::new(bus, CACHE_PATH);
        let msg = match call_raw(
            &self.conn,
            &cache_ref,
            I_CACHE,
            "GetItems",
            &(),
            BULK_TIMEOUT.min(deadline.saturating_duration_since(now)),
        )
        .await
        {
            Ok(m) => m,
            Err(e) => {
                let store = if format!("{e:#}").contains("timed out") {
                    // A bulk fetch too big for the budget (large web pages):
                    // live traversal for a while instead of paying it again.
                    warnings.push(format!(
                        "the app's bulk accessibility cache did not arrive within the observation budget; using a time-bounded live walk (retrying the cache in {} s)",
                        CACHE_RETRY.as_secs()
                    ));
                    AppCache::empty(false, Some(now + CACHE_RETRY))
                } else if is_unsupported(&e) {
                    warnings.push(
                        "application has no org.a11y.atspi.Cache; using bounded live traversal"
                            .into(),
                    );
                    AppCache::empty(false, None)
                } else {
                    warnings.push(format!(
                        "Cache.GetItems failed; using bounded live traversal: {e:#}"
                    ));
                    AppCache::empty(false, Some(now + CACHE_RETRY))
                };
                self.caches.insert(bus.to_string(), store);
                return;
            }
        };
        let body = msg.body();
        let mut nodes: HashMap<ObjRef, Node> = HashMap::new();
        let mut kids: HashMap<ObjRef, Vec<ObjRef>> = HashMap::new();
        if let Ok(items) = body.deserialize::<Vec<NewItem>>() {
            let mut grouped: HashMap<ObjRef, Vec<(i32, usize, ObjRef)>> = HashMap::new();
            for (seq, it) in items.into_iter().enumerate() {
                let r = ObjRef::from_wire(&it.0);
                let parent = ObjRef::from_wire(&it.2);
                let parent = (!parent.is_null()).then_some(parent);
                if let Some(p) = &parent {
                    grouped
                        .entry(p.clone())
                        .or_default()
                        .push((it.3, seq, r.clone()));
                }
                nodes.insert(
                    r,
                    Node {
                        parent,
                        child_count: it.4,
                        ifaces: it.5,
                        name: it.6,
                        role: it.7,
                        desc: it.8,
                        states: states_from_wire(&it.9),
                    },
                );
            }
            for (p, mut v) in grouped {
                v.sort_by_key(|(idx, seq, _)| (if *idx < 0 { i32::MAX } else { *idx }, *seq));
                kids.insert(p, v.into_iter().map(|(_, _, r)| r).collect());
            }
        } else if let Ok(items) = body.deserialize::<Vec<OldItem>>() {
            for it in items {
                let r = ObjRef::from_wire(&it.0);
                let parent = ObjRef::from_wire(&it.2);
                let children: Vec<ObjRef> =
                    it.3.iter()
                        .map(ObjRef::from_wire)
                        .filter(|c| !c.is_null())
                        .collect();
                let child_count = children.len() as i32;
                kids.insert(r.clone(), children);
                nodes.insert(
                    r,
                    Node {
                        parent: (!parent.is_null()).then_some(parent),
                        child_count,
                        ifaces: it.4,
                        name: it.5,
                        role: it.6,
                        desc: it.7,
                        states: states_from_wire(&it.8),
                    },
                );
            }
        } else {
            warnings.push(format!(
                "unrecognized Cache.GetItems signature {:?}; using bounded live traversal",
                body.signature().to_string()
            ));
            self.caches.insert(
                bus.to_string(),
                AppCache::empty(false, Some(now + CACHE_RETRY)),
            );
            return;
        }
        if nodes.is_empty() {
            warnings
                .push("Cache.GetItems returned no objects; using bounded live traversal".into());
        }
        self.caches.insert(
            bus.to_string(),
            AppCache {
                fetched_at: now,
                supported: true,
                retry_at: None,
                nodes,
                kids,
                actions: HashMap::new(),
            },
        );
    }

    /// The window's tree below `frame`, breadth first: child lists and
    /// nodes from the application's mirror where it has them, fetched live
    /// otherwise (and recorded in `tree` for the mirror).
    async fn collect_tree(
        &self,
        store: Option<&AppCache>,
        frame: &ObjRef,
        frame_node: Node,
        deadline: Instant,
    ) -> Tree {
        let conn = &self.conn;
        let mut tree = Tree::default();
        tree.nodes.insert(frame.clone(), frame_node);
        let mut frontier = vec![frame.clone()];
        let mut view: Option<Rect> = None;
        for depth in 0..MAX_DEPTH {
            if frontier.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                tree.truncated
                    .push("time budget exhausted while collecting the tree".into());
                break;
            }
            let mut next: Vec<ObjRef> = Vec::new();
            let mut lists: Vec<(ObjRef, Result<Vec<ObjRef>>)> = Vec::new();
            let mut need_live: Vec<ObjRef> = Vec::new();
            let mut big: Vec<(ObjRef, i32)> = Vec::new();
            for r in &frontier {
                let n = &tree.nodes[r];
                if (depth > 0 && !has(n.states, S_SHOWING))
                    || has(n.states, S_DEFUNCT)
                    || n.child_count == 0
                {
                    continue;
                }
                match store.and_then(|s| s.kids.get(r)) {
                    Some(k)
                        if !k.is_empty()
                            && (n.child_count < 0 || k.len() as i32 >= n.child_count) =>
                    {
                        lists.push((r.clone(), Ok(k.clone())));
                    }
                    _ if n.child_count > MAX_CHILDREN_LIVE => big.push((r.clone(), n.child_count)),
                    _ => need_live.push(r.clone()),
                }
            }
            if !need_live.is_empty() {
                let fetched: Vec<(ObjRef, Result<Vec<ObjRef>>)> = stream::iter(need_live)
                    .map(|r| async move {
                        let k = live_children(conn, &r).await;
                        (r, k)
                    })
                    .buffer_unordered(CONCURRENCY)
                    .take_until(tokio::time::sleep_until(deadline.into()))
                    .collect()
                    .await;
                lists.extend(fetched);
            }
            for (r, count) in big {
                if view.is_none() {
                    view = extents(conn, frame, COORD_SCREEN).await.ok().flatten();
                }
                let k = tokio::time::timeout_at(
                    deadline.into(),
                    visible_children(conn, &r, count, view),
                )
                .await
                .unwrap_or_else(|_| Err(anyhow!("timed out")));
                let n = &tree.nodes[&r];
                tree.truncated.push(match &k {
                    Ok((first, k)) if !k.is_empty() => format!(
                        "{} {} has {count} children; showing {}-{} (the visible part); scroll for more",
                        role_name(n.role),
                        quote(&n.name, 40),
                        first + 1,
                        first + k.len() as i32
                    ),
                    _ => format!(
                        "{count} children of a {} {} not traversed (exceeds {MAX_CHILDREN_LIVE})",
                        role_name(n.role),
                        quote(&n.name, 40)
                    ),
                });
                tree.partial.insert(r.clone());
                lists.push((r, k.map(|(_, k)| k)));
            }
            let mut to_fetch: Vec<ObjRef> = Vec::new();
            let mut queued: std::collections::HashSet<ObjRef> = std::collections::HashSet::new();
            let mut new_lists: Vec<(ObjRef, Vec<ObjRef>)> = Vec::new();
            for (p, k) in lists {
                match k {
                    Ok(k) => {
                        for c in &k {
                            if tree.nodes.contains_key(c) || queued.contains(c) {
                                continue;
                            }
                            if let Some(cn) = store.and_then(|s| s.nodes.get(c)) {
                                if tree.nodes.len() < MAX_TREE_NODES {
                                    tree.nodes.insert(c.clone(), cn.clone());
                                    tree.cached_nodes += 1;
                                    next.push(c.clone());
                                }
                            } else {
                                queued.insert(c.clone());
                                to_fetch.push(c.clone());
                            }
                        }
                        new_lists.push((p, k));
                    }
                    Err(_) => tree.failures += 1,
                }
            }
            if !to_fetch.is_empty() {
                let room = MAX_LIVE_NODES
                    .saturating_sub(tree.live_nodes)
                    .min(MAX_TREE_NODES.saturating_sub(tree.nodes.len()));
                if to_fetch.len() > room {
                    tree.truncated.push(format!(
                        "{} objects not fetched (live traversal limit {MAX_LIVE_NODES}/{MAX_TREE_NODES} nodes)",
                        to_fetch.len() - room
                    ));
                    to_fetch.truncate(room);
                }
                let fetched: Vec<(ObjRef, Result<Node>)> = stream::iter(to_fetch)
                    .map(|r| async move {
                        let n = fetch_node_live(conn, &r).await;
                        (r, n)
                    })
                    .buffer_unordered(CONCURRENCY)
                    .take_until(tokio::time::sleep_until(deadline.into()))
                    .collect()
                    .await;
                for (r, n) in fetched {
                    match n {
                        Ok(n) => {
                            tree.live_nodes += 1;
                            tree.nodes.insert(r.clone(), n);
                            next.push(r);
                        }
                        Err(_) => tree.failures += 1,
                    }
                }
            }
            for (p, k) in new_lists {
                let k: Vec<ObjRef> = k
                    .into_iter()
                    .filter(|c| tree.nodes.contains_key(c))
                    .collect();
                tree.kids.insert(p, k);
            }
            if tree.nodes.len() >= MAX_TREE_NODES && !next.is_empty() {
                tree.truncated
                    .push(format!("tree truncated at {MAX_TREE_NODES} objects"));
                // Keep already-collected nodes but stop descending.
                frontier.clear();
                break;
            }
            frontier = next;
        }
        if !frontier.is_empty() && tree.truncated.is_empty() {
            tree.truncated
                .push(format!("tree depth limited to {MAX_DEPTH}"));
        }
        tree
    }

    async fn snapshot_inner(
        &mut self,
        w: &WindowInfo,
        warnings: &mut Vec<String>,
    ) -> Result<String> {
        let mut trace = crate::Trace::new(format!("snapshot {}", w.window_id));
        let snap_seq = self.event_seq();
        self.obs.begin(w);

        let (frame, frame_node) = self.map_window(w, warnings).await?;
        if self.obs.bind_frame(w.window_id, &frame) {
            warnings.push("window maps to a different accessible frame than before; earlier element IDs are no longer valid".into());
        }
        trace.mark("map");
        // The budget starts once the app answers: the tree walk leaves part
        // of it for geometry and details.
        let deadline = Instant::now() + SNAPSHOT_BUDGET;
        let walk_deadline = deadline
            .checked_sub(RENDER_RESERVE)
            .unwrap_or(deadline)
            .max(Instant::now());
        self.app_store(&frame.bus, walk_deadline, warnings).await;
        trace.mark("cache");
        let store = self.caches.get(&frame.bus);
        let tree = self
            .collect_tree(store, &frame, frame_node.clone(), walk_deadline)
            .await;
        trace.mark("tree");
        trace.note("nodes", tree.nodes.len());
        trace.note("live", tree.live_nodes);
        let mirrored = store.is_some();
        // What was fetched live joins the mirror for the next observation.
        if let Some(store) = self.caches.get_mut(&frame.bus) {
            store.nodes.insert(frame.clone(), frame_node);
            for (r, n) in &tree.nodes {
                store.nodes.entry(r.clone()).or_insert_with(|| n.clone());
            }
            for (p, k) in &tree.kids {
                if !tree.partial.contains(p) && !store.kids.contains_key(p) {
                    store.kids.insert(p.clone(), k.clone());
                }
            }
        }
        let conn = &self.conn;

        let (ww, wh) = (w.width as i32, w.height as i32);
        // Pass 1: candidates (no detail-dependent decisions). Geometry is
        // not known yet, so subtrees outside the window are not pruned here;
        // the few details fetched for them cost less than a second round of
        // calls after the geometry.
        let mut cands: Vec<ObjRef> = Vec::new();
        walk(
            &tree,
            &HashMap::new(),
            &frame,
            ww,
            wh,
            &mut 0,
            &mut |r, n, _| {
                if cands.len() >= MAX_CANDIDATES {
                    return false;
                }
                let pre = r == &frame || interesting(n, None);
                if pre {
                    cands.push(r.clone());
                }
                pre
            },
        );

        // Geometry for every collected object (window-relative) and details
        // for the candidates, in one round of concurrent calls. Screen
        // coordinates relative to the X window's origin: unlike COORD_WINDOW
        // (relative to each object's own toplevel), this is also right for
        // menus and other popups.
        enum Fetch {
            Rect(ObjRef),
            Detail(ObjRef, Node, Option<Vec<String>>),
        }
        enum Fetched {
            Rect(ObjRef, Option<Rect>),
            Detail(ObjRef, Details),
        }
        let (ox, oy) = w.origin;
        let known_actions = self.caches.get(&frame.bus).map(|s| &s.actions);
        // Built eagerly: an iterator holding borrows across the awaits below
        // would keep the observation future from being Send.
        let mut fetches: Vec<Fetch> = Vec::new();
        for (r, n) in &tree.nodes {
            if n.has_iface(I_COMPONENT) && has(n.states, S_SHOWING) {
                fetches.push(Fetch::Rect(r.clone()));
            }
        }
        for r in cands {
            if let Some(n) = tree.nodes.get(&r) {
                let actions = known_actions.and_then(|a| a.get(&r)).cloned();
                fetches.push(Fetch::Detail(r, n.clone(), actions));
            }
        }
        let fetched: Vec<Fetched> = stream::iter(fetches)
            .map(|f| async move {
                match f {
                    Fetch::Rect(r) => {
                        let e = extents(conn, &r, COORD_SCREEN).await.ok().flatten();
                        Fetched::Rect(
                            r,
                            e.map(|e| Rect {
                                x: e.x - ox,
                                y: e.y - oy,
                                ..e
                            }),
                        )
                    }
                    Fetch::Detail(r, n, actions) => {
                        let d = fetch_details(conn, &r, &n, actions).await;
                        Fetched::Detail(r, d)
                    }
                }
            })
            .buffer_unordered(CONCURRENCY)
            .take_until(tokio::time::sleep_until(deadline.into()))
            .collect()
            .await;
        let mut rects: HashMap<ObjRef, Rect> = HashMap::new();
        let mut details: HashMap<ObjRef, Details> = HashMap::new();
        for f in fetched {
            match f {
                Fetched::Rect(r, Some(e)) => {
                    rects.insert(r, e);
                }
                Fetched::Rect(_, None) => {}
                Fetched::Detail(r, d) => {
                    details.insert(r, d);
                }
            }
        }
        if let Some(store) = self.caches.get_mut(&frame.bus) {
            for (r, d) in &details {
                if let Some(a) = &d.actions {
                    store.actions.entry(r.clone()).or_insert_with(|| a.clone());
                }
            }
        }
        trace.mark("rects+details");
        trace.note("comps", rects.len());
        trace.note("cands", details.len());

        // Pass 2: render.
        let obs = &mut self.obs;
        let mut lines: Vec<(usize, u32, String)> = Vec::new();
        let mut elements: HashMap<u32, Elem> = HashMap::new();
        let mut line_truncated = false;
        let mut offscreen = 0usize;
        walk(
            &tree,
            &rects,
            &frame,
            ww,
            wh,
            &mut offscreen,
            &mut |r, n, depth| {
                let d = details.get(r);
                let show = r == &frame || interesting(n, d);
                if !show {
                    return false;
                }
                if lines.len() >= MAX_LINES {
                    line_truncated = true;
                    return false;
                }
                let id = obs.id_for(w.window_id, r, n.role);
                lines.push((depth, id, render_line(n, d, rects.get(r), ww, wh)));
                elements.insert(
                    id,
                    Elem {
                        r: r.clone(),
                        role: n.role,
                        name: n.name.clone(),
                        ifaces: n.ifaces.clone(),
                        states: n.states,
                    },
                );
                true
            },
        );

        let source = match (mirrored, tree.live_nodes) {
            (true, 0) => "cached".to_string(),
            (true, n) => format!("cached + {n} live"),
            (false, n) => format!("live traversal ({n} objects)"),
        };
        let mut text = format!(
            "# accessibility: {} elements shown of {} objects ({source}); coordinates are window-local\n",
            lines.len(),
            tree.nodes.len()
        );
        for (depth, id, body) in &lines {
            text.push_str(&"  ".repeat((*depth).min(40)));
            text.push_str(&format!("[{id}] {body}\n"));
        }

        if offscreen > 0 {
            warnings.push(format!(
                "{offscreen} subtrees outside the window bounds were omitted"
            ));
        }
        if line_truncated {
            warnings.push(format!(
                "accessibility text truncated at {MAX_LINES} elements"
            ));
        }
        if details.len() < lines.len() {
            warnings.push("some element details were not fetched within the time budget".into());
        }
        if tree.failures > 0 {
            warnings.push(format!(
                "{} accessibility queries failed (objects may have vanished)",
                tree.failures
            ));
        }
        warnings.extend(tree.truncated.iter().cloned());
        // Hints ("hint: " prefix) are repeated on every observation; the
        // host shows each one once, when the model actually receives it.
        if let Some(h) = empty_item_view_hint(&tree) {
            warnings.push(format!("hint: {h}"));
        }

        // Number fields of an app with a decimal-comma locale: say so up
        // front (once per process) rather than after a reformatted input.
        if elements
            .values()
            .any(|e| matches!(e.role, R_SPIN_BUTTON | R_EDITBAR))
        {
            if let Some(h) = numeric_locale(w.pid).and_then(|l| decimal_hint(&l, "125.50")) {
                warnings.push(format!(
                    "hint: number input: {}",
                    h.replace(
                        "type 125,50 rather than 125.50",
                        "type numbers like 125,50, not 125.50"
                    )
                ));
            }
        }
        let obs = &mut self.obs;
        let ws = obs.windows.get_mut(&w.window_id).expect("window state");
        ws.elements = elements;
        ws.snap_seq = snap_seq;
        ws.valid = true;
        self.publish_floor();
        trace.mark("render");
        trace.note("lines", lines.len());
        trace.done();
        Ok(text)
    }

    // -----------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------

    /// Perform a semantic accessibility action (click, set_value,
    /// perform_secondary_action). Errors are never converted into
    /// synthesized input. A window without a current observation, or an
    /// element issued by an earlier observation of it, is observed first.
    pub async fn action(&mut self, window: &WindowInfo, method: &str, p: &Value) -> Result<()> {
        let idx = Self::element_index(p).ok();
        self.ensure_observed(window, idx).await;
        let r = match method {
            "click" => self.do_click(window, p).await,
            "set_value" => self.do_set_value(window, p).await,
            "perform_secondary_action" => self.do_secondary(window, p).await,
            other => bail!("accessibility backend does not implement `{other}`"),
        };
        // An action call that timed out is still running inside the app (a
        // nested main loop, e.g. a modal dialog): its accessibility
        // connection is confirmed blocked until it answers again.
        if let Err(e) = &r {
            if format!("{e:#}").contains("timed out") {
                if let Some(bus) = idx.and_then(|i| self.obs.bus_of(window.window_id, i)) {
                    self.blocked.insert(bus);
                }
            }
        }
        r
    }

    /// Inserts `text` at the focused editable text of the window (or the
    /// given element). `NoTarget` when nothing editable has focus.
    pub async fn text_input(
        &mut self,
        window: &WindowInfo,
        text: &str,
        element_index: Option<u32>,
    ) -> Result<TextInput> {
        let mut trace = crate::Trace::new("text_input");
        self.ensure_observed(window, element_index).await;
        trace.mark("ensure");
        let el = match element_index {
            Some(_) => self.focused_editable(window, element_index).await?,
            None => {
                let mut found = self.focused_editable(window, None).await;
                // The focused field may be newer than the window's
                // observation (the location entry a shortcut just showed in
                // a file chooser): observe again once after an action.
                if found.is_err() && self.stale(window) {
                    self.observe_now(window).await;
                    trace.mark("reobserve");
                    found = self.focused_editable(window, None).await;
                }
                match found {
                    Ok(el) => el,
                    Err(e) => {
                        trace.mark("focused");
                        trace.note("target", "none");
                        trace.done();
                        return Ok(TextInput::NoTarget(format!("{e:#}")));
                    }
                }
            }
        };
        trace.mark("focused");
        if text.contains('\n') && has(el.states, S_SINGLE_LINE) {
            trace.done();
            return Ok(TextInput::SingleLine);
        }
        let mut warnings = Vec::new();
        if let Some(w) = insert_text(&self.conn, &el, text).await? {
            warnings.push(w);
            if let Some(hint) = locale_hint(window.pid, text) {
                warnings.push(hint);
            }
        }
        trace.mark("insert");
        trace.done();
        Ok(TextInput::Inserted(warnings))
    }

    /// How to click element `idx`. A pointer click whenever the element has
    /// visible geometry and the toolkit does not report another element
    /// covering its centre: toolkits run DoAction inside the accessibility
    /// call, and a handler that opens a modal dialog (gtk_dialog_run, as GTK
    /// menu items and buttons do) then blocks the app's accessibility
    /// connection, and with it all key input, until the dialog closes.
    /// DoAction for elements without visible geometry (scrolled out of view)
    /// or whose centre is covered (an overlay, a sticky header); a menu item
    /// in either case is refused, its menu is not open or the click would hit
    /// something else. A toolkit that does not answer the hit test, or
    /// answers with a sibling (GTK tree views), does not stop the click.
    pub async fn click_plan(&mut self, window: &WindowInfo, idx: u32) -> Result<ClickPlan> {
        let mut trace = crate::Trace::new(format!("click_plan {idx}"));
        self.ensure_observed(window, Some(idx)).await;
        trace.mark("ensure");
        let el = self.resolve(window, idx).await?;
        trace.mark("resolve");
        require_enabled(&el, idx)?;
        let menu_item = matches!(el.role, R_MENU_ITEM | R_CHECK_MENU_ITEM | R_RADIO_MENU_ITEM);
        let geometry = if el.has_iface(I_COMPONENT) {
            click_geometry(&self.conn, &el, menu_item).await?
        } else {
            None
        };
        trace.mark("geometry");
        let Some(g) = geometry else {
            if menu_item {
                bail!("menu item {idx} has no on-screen geometry (its menu is closed or it is scrolled out of view); open or scroll its menu first");
            }
            return Ok(ClickPlan::Action { warning: None });
        };
        let hit = hit_at_point(&self.conn, &el.r, &g).await;
        trace.mark("hit");
        trace.note("hit", format!("{hit:?}").split('(').next().unwrap_or("?"));
        trace.done();
        if let PointHit::Other(o) = &hit {
            let what = format!("its centre is covered by {o}");
            if menu_item {
                bail!("menu item {idx}: {what}; refusing to click (close what covers it, or scroll the menu)");
            }
            return Ok(ClickPlan::Action {
                warning: Some(format!("element {idx}: {what}, so it was activated through accessibility instead of a pointer click")),
            });
        }
        let (x, y) = g.center;
        // Items of an open menu, and elements of popup windows (completion
        // lists, dropdowns), lie on the app's popup windows; an item of a
        // menu bar lies in the window itself.
        let popup = if menu_item {
            parent_role(&self.conn, &el.r).await != Some(R_MENU_BAR)
        } else {
            g.top_role.is_some_and(is_popup_role)
        };
        let r = g.rect;
        Ok(ClickPlan::Pointer {
            x,
            y,
            popup: popup.then_some((r.x, r.y, r.w, r.h)),
        })
    }

    /// Screen centre of a text field that exposes Text but not EditableText
    /// (Chromium web inputs), which can only be set with key events.
    pub async fn key_entry_point(
        &mut self,
        window: &WindowInfo,
        idx: u32,
    ) -> Result<Option<(i32, i32)>> {
        self.ensure_observed(window, Some(idx)).await;
        let el = self.resolve(window, idx).await?;
        if el.has_iface(I_EDIT)
            || !el.has_iface(I_TEXT)
            || !el.has_iface(I_COMPONENT)
            || !matches!(el.role, R_ENTRY | R_PASSWORD_TEXT)
        {
            return Ok(None);
        }
        require_enabled(&el, idx)?;
        let Some(r) = extents(&self.conn, &el.r, COORD_SCREEN).await? else {
            return Ok(None);
        };
        if r.w <= 0 || r.h <= 0 {
            return Ok(None);
        }
        Ok(Some((r.x + r.w.min(40) / 2, r.y + r.h / 2)))
    }

    /// Whether element `idx` (already resolved for this action) has focus
    /// and is writable, read live; polls briefly because focus follows a
    /// click asynchronously.
    pub async fn focused_writable(&self, w: &WindowInfo, idx: u32) -> Result<bool> {
        let r = self
            .obs
            .windows
            .get(&w.window_id)
            .and_then(|ws| ws.elements.get(&idx))
            .map(|e| e.r.clone())
            .ok_or_else(|| anyhow!("element {idx} is not in the latest observation"))?;
        let until = Instant::now() + Duration::from_millis(300);
        loop {
            let st: Vec<u32> = call(&self.conn, &r, I_ACC, "GetState", &()).await?;
            let st = states_from_wire(&st);
            if has(st, S_FOCUSED) && !has(st, S_READ_ONLY) {
                return Ok(true);
            }
            if Instant::now() >= until {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Current text of an element with the Text interface.
    pub async fn element_text(&mut self, window: &WindowInfo, idx: u32) -> Result<String> {
        let el = self.resolve(window, idx).await?;
        let n: i32 = get_prop(&self.conn, &el.r, I_TEXT, "CharacterCount").await?;
        call(&self.conn, &el.r, I_TEXT, "GetText", &(0i32, n.min(4000))).await
    }

    /// Reason not to send key events to the window's app, if any. GTK's
    /// accessibility bridge makes a synchronous call to the accessibility bus
    /// for every key event; while the app's connection is blocked inside an
    /// earlier accessibility call (a modal dialog run from within DoAction),
    /// that call never returns and the app hangs for good. So an app that
    /// does not answer a quick probe gets no key events.
    ///
    /// Only a confirmed block refuses: an earlier action call to the app timed
    /// out and the app still does not answer. A merely busy app is probed
    /// again for up to `KEY_BUSY_WAIT` and then gets the keys (queued) with a
    /// warning. `Err` is the refusal, `Ok(Some)` a warning.
    pub async fn key_input_gate(
        &mut self,
        w: &WindowInfo,
    ) -> std::result::Result<Option<String>, String> {
        let mut trace = crate::Trace::new("key_gate");
        let r = self.key_input_gate_inner(w).await;
        trace.mark("probe");
        trace.done();
        r
    }

    async fn key_input_gate_inner(
        &mut self,
        w: &WindowInfo,
    ) -> std::result::Result<Option<String>, String> {
        let Some(bus) = self
            .obs
            .windows
            .get(&w.window_id)
            .filter(|ws| ws.pid == w.pid)
            .or_else(|| {
                self.obs
                    .windows
                    .values()
                    .find(|ws| ws.pid == w.pid && ws.frame.is_some())
            })
            .and_then(|ws| ws.frame.as_ref())
            .map(|f| f.bus.clone())
        else {
            return Ok(None);
        };
        let confirmed = self.blocked.contains(&bus);
        let until = Instant::now()
            + if confirmed {
                KEY_PROBE_TIMEOUT
            } else {
                KEY_BUSY_WAIT
            };
        loop {
            match self.probe(&bus, KEY_PROBE_TIMEOUT).await {
                Some(true) => return Ok(None),
                Some(false) if Instant::now() < until => continue,
                Some(false) => break,
                None => return Ok(None),
            }
        }
        if confirmed {
            Err("the app's accessibility connection is blocked: an earlier accessibility action is still running inside it (typically a modal dialog it opened). Key events would deadlock it, so none were sent. Answer the dialog with a pointer click on its buttons (coordinates from a screenshot) first".into())
        } else {
            Ok(Some(format!(
                "the app did not answer accessibility requests for {} ms (busy); the keys were sent and will be handled when it is free",
                KEY_BUSY_WAIT.as_millis()
            )))
        }
    }

    /// Quick liveness probe of an app's accessibility connection: Some(true)
    /// answered (clears the unresponsive/blocked marks), Some(false) timed
    /// out, None another error.
    async fn probe(&mut self, bus: &str, timeout: Duration) -> Option<bool> {
        let app = ObjRef::new(bus, REGISTRY_ROOT);
        let r: Result<Vec<u32>> = call_t(&self.conn, &app, I_ACC, "GetState", &(), timeout).await;
        match r {
            Ok(_) => {
                self.unresponsive.remove(bus);
                self.blocked.remove(bus);
                Some(true)
            }
            Err(e) if format!("{e:#}").contains("timed out") => {
                self.unresponsive
                    .insert(bus.to_string(), Instant::now() + UNRESPONSIVE_RETRY);
                Some(false)
            }
            Err(_) => None,
        }
    }

    /// Whether an app marked unresponsive is still unresponsive: within the
    /// mark's window a brief re-probe instead of skipping it outright.
    async fn still_unresponsive(&mut self, bus: &str) -> bool {
        if !self
            .unresponsive
            .get(bus)
            .is_some_and(|until| Instant::now() < *until)
        {
            return false;
        }
        self.probe(bus, REPROBE_TIMEOUT).await == Some(false)
    }

    /// Observes the window implicitly when it has no current observation, or
    /// when `idx` came from an earlier observation of it (its object may
    /// still be there and keeps its ID).
    async fn ensure_observed(&mut self, w: &WindowInfo, idx: Option<u32>) {
        let need = match self.obs.windows.get(&w.window_id) {
            None => true,
            Some(ws) => {
                !ws.valid
                    || ws.pid != w.pid
                    || idx.is_some_and(|i| {
                        !ws.elements.contains_key(&i) && ws.ids.values().any(|&(id, _)| id == i)
                    })
            }
        };
        if need {
            self.observe_now(w).await;
        }
    }

    /// A bounded snapshot whose warnings nobody reads: it refreshes the
    /// window's element map (or leaves the previous one when it fails).
    async fn observe_now(&mut self, w: &WindowInfo) {
        let mut ignored = Vec::new();
        let r = tokio::time::timeout(SNAPSHOT_BACKSTOP, self.snapshot_inner(w, &mut ignored)).await;
        let _ = r;
    }

    /// Marks the start of an action: observations taken before it may not
    /// show the elements the action creates (actions do not observe).
    pub fn begin_action(&mut self) {
        self.last_action_seq = self.event_seq();
    }

    /// Whether the window's latest observation predates the last action.
    fn stale(&self, w: &WindowInfo) -> bool {
        self.obs
            .windows
            .get(&w.window_id)
            .is_none_or(|ws| !ws.valid || ws.snap_seq <= self.last_action_seq)
    }

    /// Window-local centre of a still-valid element from the latest observation,
    /// for coordinate-based operations such as element scroll.
    pub async fn element_point(
        &mut self,
        window: &WindowInfo,
        element_index: u32,
    ) -> Result<(i32, i32)> {
        self.ensure_observed(window, Some(element_index)).await;
        let el = self.resolve(window, element_index).await?;
        if !el.has_iface(I_COMPONENT) {
            bail!("element {element_index} has no geometry (no Component interface)");
        }
        let r = extents(&self.conn, &el.r, COORD_SCREEN)
            .await?
            .ok_or_else(|| anyhow!("element {element_index} reports no geometry"))?;
        let r = Rect {
            x: r.x - window.origin.0,
            y: r.y - window.origin.1,
            ..r
        };
        let (ww, wh) = (window.width as i32, window.height as i32);
        if r.w <= 0 || r.h <= 0 {
            bail!("element {element_index} has zero size");
        }
        let x0 = r.x.max(0);
        let y0 = r.y.max(0);
        let x1 = (r.x.saturating_add(r.w)).min(ww);
        let y1 = (r.y.saturating_add(r.h)).min(wh);
        if x1 <= x0 || y1 <= y0 {
            bail!("element {element_index} is outside the window bounds");
        }
        Ok(((x0 + x1 - 1) / 2, (y0 + y1 - 1) / 2))
    }

    fn element_index(p: &Value) -> Result<u32> {
        let v = p
            .get("element_index")
            .ok_or_else(|| anyhow!("missing `element_index`"))?;
        let n = v
            .as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            .ok_or_else(|| anyhow!("`element_index` must be a non-negative integer"))?;
        u32::try_from(n).map_err(|_| anyhow!("`element_index` out of range"))
    }

    /// Resolve an element ID against the latest observation of this window and
    /// revalidate it live. Rejects anything stale.
    async fn resolve(&mut self, w: &WindowInfo, idx: u32) -> Result<Elem> {
        let ws = self.obs.windows.get(&w.window_id).ok_or_else(|| {
            anyhow!(
                "no accessibility observation of window {}; observe it first",
                w.window_id
            )
        })?;
        if ws.pid != w.pid {
            bail!(
                "window {} changed owner since the last observation; observe it again",
                w.window_id
            );
        }
        if !ws.valid {
            bail!(
                "the latest observation of window {} has no usable accessibility tree",
                w.window_id
            );
        }
        let el = ws.elements.get(&idx).cloned().ok_or_else(|| {
            anyhow!(
                "element {idx} is not in the latest observation of window {}",
                w.window_id
            )
        })?;
        let frame = ws
            .frame
            .clone()
            .ok_or_else(|| anyhow!("window has no mapped accessible frame"))?;
        let snap_seq = ws.snap_seq;
        {
            let log = self
                .events
                .lock()
                .map_err(|_| anyhow!("event log poisoned"))?;
            if snap_seq < log.lost_before {
                bail!("too many accessibility changes since the last observation; observe again");
            }
            if log.gone.contains_key(&el.r.bus) {
                bail!("the application owning element {idx} has disconnected from the accessibility bus");
            }
            if let Some(&s) = log.removed.get(&el.r) {
                if s > snap_seq {
                    bail!("element {idx} was removed after the last observation; observe again");
                }
            }
        }
        if self.still_unresponsive(&el.r.bus.clone()).await {
            bail!("the app's accessibility interface is not responding; element {idx} cannot be used until it does (pointer input still works)");
        }
        let conn = &self.conn;
        let node = fetch_node_live(conn, &el.r)
            .await
            .with_context(|| format!("element {idx} is no longer reachable; observe again"))?;
        if has(node.states, S_DEFUNCT) {
            bail!("element {idx} is defunct; observe again");
        }
        if node.role != el.role {
            bail!(
                "element {idx} changed role ({} -> {}); observe again",
                role_name(el.role),
                role_name(node.role)
            );
        }
        if node.name != el.name {
            bail!(
                "element {idx} changed name ({} -> {}); observe again",
                quote(&el.name, 60),
                quote(&node.name, 60)
            );
        }
        if !has(node.states, S_SHOWING) {
            bail!("element {idx} is no longer showing; observe again");
        }
        // Window binding: the element must still descend from the mapped frame.
        let mut cur = el.r.clone();
        let mut parent = node.parent.clone();
        let mut ok = cur == frame;
        for _ in 0..(MAX_DEPTH + 16) {
            if ok {
                break;
            }
            let Some(p) = parent.take() else { break };
            if p == frame {
                ok = true;
                break;
            }
            cur = p;
            let pr: Result<(String, OwnedObjectPath)> = get_prop(conn, &cur, I_ACC, "Parent").await;
            parent = pr
                .ok()
                .map(|w| ObjRef::from_wire(&w))
                .filter(|r| !r.is_null());
        }
        if !ok {
            bail!(
                "element {idx} is no longer inside window {}; observe again",
                w.window_id
            );
        }
        Ok(Elem {
            states: node.states,
            ifaces: node.ifaces,
            ..el
        })
    }

    async fn do_click(&mut self, w: &WindowInfo, p: &Value) -> Result<()> {
        if p.get("element_index").is_none() {
            bail!("coordinate click is not an accessibility action");
        }
        let idx = Self::element_index(p)?;
        let clicks = p
            .get("click_count")
            .or_else(|| p.get("clicks"))
            .and_then(Value::as_u64)
            .unwrap_or(1);
        if clicks != 1 {
            bail!("semantic click supports click_count 1 only (got {clicks})");
        }
        let button = p
            .get("button")
            .and_then(Value::as_str)
            .unwrap_or("left")
            .to_ascii_lowercase();
        let el = self.resolve(w, idx).await?;
        require_enabled(&el, idx)?;
        let conn = &self.conn;
        match button.as_str() {
            "left" | "primary" => {}
            "right" | "secondary" | "context" => {
                let acts = action_names(conn, &el).await?;
                let i = acts
                    .iter()
                    .position(|(n, l)| {
                        let n = format!("{n} {l}").to_ascii_lowercase();
                        n.contains("menu") || n.contains("popup") || n.contains("context")
                    })
                    .ok_or_else(|| {
                        anyhow!(
                            "element {idx} exposes no context-menu action (actions: {})",
                            list_actions(&acts)
                        )
                    })?;
                return do_action(conn, &el, i as i32).await;
            }
            other => bail!("semantic click does not support button {other:?}"),
        }
        let acts = action_names(conn, &el).await?;
        if let Some(i) = preferred_action(&acts, &CLICK_ACTIONS) {
            return do_action(conn, &el, i as i32).await;
        }
        // Never fall back to an arbitrary action: index 0 may be "delete".
        if has(el.states, S_SELECTABLE) {
            if let Ok(()) = select_in_parent(conn, &el).await {
                return Ok(());
            }
        }
        if el.has_iface(I_COMPONENT) && has(el.states, S_FOCUSABLE) && el.has_iface(I_EDIT) {
            let ok: bool =
                call_t(conn, &el.r, I_COMPONENT, "GrabFocus", &(), ACTION_TIMEOUT).await?;
            if !ok {
                bail!("application refused to focus element {idx}");
            }
            return Ok(());
        }
        bail!(
            "element {idx} ({}) has no click/press/activate action (available actions: {}); use perform_secondary_action with one of them, or window coordinates",
            role_name(el.role),
            list_actions(&acts)
        )
    }

    async fn do_secondary(&mut self, w: &WindowInfo, p: &Value) -> Result<()> {
        let idx = Self::element_index(p)?;
        let want = p
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("perform_secondary_action requires string `action`"))?
            .trim()
            .to_string();
        let el = self.resolve(w, idx).await?;
        require_enabled(&el, idx)?;
        let conn = &self.conn;
        let acts = if el.has_iface(I_ACTION) {
            action_names(conn, &el).await?
        } else {
            Vec::new()
        };
        if let Some(i) = acts
            .iter()
            .position(|(n, l)| n.eq_ignore_ascii_case(&want) || l.eq_ignore_ascii_case(&want))
        {
            return do_action(conn, &el, i as i32).await;
        }
        match want.to_ascii_lowercase().replace([' ', '-'], "_").as_str() {
            "focus" | "grab_focus" if el.has_iface(I_COMPONENT) => {
                let ok: bool =
                    call_t(conn, &el.r, I_COMPONENT, "GrabFocus", &(), ACTION_TIMEOUT).await?;
                if !ok {
                    bail!("application refused to focus element {idx}");
                }
                Ok(())
            }
            "scroll_into_view" | "scroll_to" | "show" if el.has_iface(I_COMPONENT) => {
                let ok: bool = call_t(
                    conn,
                    &el.r,
                    I_COMPONENT,
                    "ScrollTo",
                    &(SCROLL_ANYWHERE,),
                    ACTION_TIMEOUT,
                )
                .await?;
                if !ok {
                    bail!("application could not scroll element {idx} into view");
                }
                Ok(())
            }
            "select" if has(el.states, S_SELECTABLE) => select_in_parent(conn, &el).await,
            _ => bail!(
                "element {idx} has no action {want:?} (available: {}{})",
                list_actions(&acts),
                if el.has_iface(I_COMPONENT) {
                    ", focus, scroll_into_view"
                } else {
                    ""
                }
            ),
        }
    }

    async fn do_set_value(&mut self, w: &WindowInfo, p: &Value) -> Result<()> {
        let idx = Self::element_index(p)?;
        let value = p
            .get("value")
            .ok_or_else(|| anyhow!("set_value requires `value`"))?
            .clone();
        let el = self.resolve(w, idx).await?;
        require_enabled(&el, idx)?;
        let conn = &self.conn;

        if is_toggle_role(el.role) || (has(el.states, S_CHECKABLE) && !el.has_iface(I_EDIT)) {
            let want = parse_bool(&value).ok_or_else(|| {
                anyhow!(
                    "set_value on a {} needs a boolean value",
                    role_name(el.role)
                )
            })?;
            let cur = has(el.states, S_CHECKED);
            if cur == want {
                return Ok(());
            }
            if !want && matches!(el.role, R_RADIO_BUTTON | R_RADIO_MENU_ITEM) {
                bail!("a radio button cannot be unchecked directly; select another option");
            }
            let acts = action_names(conn, &el).await?;
            let i = preferred_action(&acts, &TOGGLE_ACTIONS).ok_or_else(|| {
                    anyhow!(
                        "element {idx} exposes no toggle/click/press/activate action (available actions: {})",
                        list_actions(&acts)
                    )
                })?;
            do_action(conn, &el, i as i32).await?;
            let until = Instant::now() + TOGGLE_VERIFY;
            loop {
                let s = live_states(conn, &el.r).await?;
                if has(s, S_CHECKED) == want {
                    return Ok(());
                }
                if Instant::now() >= until {
                    bail!(
                        "toggle action was accepted but element {idx} is still {}",
                        if cur { "checked" } else { "unchecked" }
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        // GTK spin buttons expose both EditableText and Value. Editing their
        // text only changes a pending string; CurrentValue commits the number
        // and notifies the application immediately, without focus/Return.
        if el.has_iface(I_EDIT) && !(el.role == R_SPIN_BUTTON && el.has_iface(I_VALUE)) {
            let text = match &value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => bail!("set_value on text needs a string value"),
            };
            require_editable(&el, idx)?;
            if text.len() > MAX_INSERT_BYTES {
                bail!("value too large ({} bytes)", text.len());
            }
            let ok: bool = call_t(
                conn,
                &el.r,
                I_EDIT,
                "SetTextContents",
                &(text.as_str(),),
                ACTION_TIMEOUT,
            )
            .await?;
            if !ok {
                bail!("application rejected SetTextContents on element {idx}");
            }
            if el.has_iface(I_TEXT) {
                let n: i32 = get_prop(conn, &el.r, I_TEXT, "CharacterCount").await?;
                let got: String = call(conn, &el.r, I_TEXT, "GetText", &(0i32, n)).await?;
                if got != text {
                    // The app reformatted the value (a decimal comma, a
                    // length limit): reported, not an error.
                    self.action_warnings.push(format!(
                        "the application changed the value: set {}, the field now contains {}",
                        quote(&text, 80),
                        quote(&got, 80)
                    ));
                    if let Some(h) = locale_hint(w.pid, &text) {
                        self.action_warnings.push(h);
                    }
                }
            }
            return Ok(());
        }

        if el.has_iface(I_VALUE) {
            let v = match &value {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            }
            .filter(|v| v.is_finite())
            .ok_or_else(|| {
                anyhow!(
                    "set_value on a {} needs a numeric value",
                    role_name(el.role)
                )
            })?;
            let props: HashMap<String, OwnedValue> =
                call(conn, &el.r, PROPS, "GetAll", &(I_VALUE,)).await?;
            let min = pv::<f64>(&props, "MinimumValue");
            let max = pv::<f64>(&props, "MaximumValue");
            if let (Some(lo), Some(hi)) = (min, max) {
                if hi >= lo && (v < lo || v > hi) {
                    bail!(
                        "value {} outside range {}..{}",
                        fmt_num(v),
                        fmt_num(lo),
                        fmt_num(hi)
                    );
                }
            }
            call_raw(
                conn,
                &el.r,
                PROPS,
                "Set",
                &(I_VALUE, "CurrentValue", zbus::zvariant::Value::from(v)),
                ACTION_TIMEOUT,
            )
            .await?;
            let got: f64 = get_prop(conn, &el.r, I_VALUE, "CurrentValue").await?;
            let step = pv::<f64>(&props, "MinimumIncrement").unwrap_or(0.0).abs();
            let tol = (step / 2.0).max(1e-9 * v.abs().max(1.0));
            if (got - v).abs() > tol.max(1e-6) {
                bail!(
                    "value not applied: requested {}, element reports {}",
                    fmt_num(v),
                    fmt_num(got)
                );
            }
            return Ok(());
        }

        if el.has_iface(I_SELECTION) {
            let want = match &value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => bail!(
                    "set_value on a {} needs the option name as a string",
                    role_name(el.role)
                ),
            };
            let mut items = live_children(conn, &el.r).await?;
            if items.len() == 1 {
                if let Ok(role) = call::<_, u32>(conn, &items[0], I_ACC, "GetRole", &()).await {
                    if matches!(role, R_MENU | R_POPUP_MENU | R_LIST | R_LIST_BOX) {
                        items = live_children(conn, &items[0]).await?;
                    }
                }
            }
            items.truncate(2000);
            let names: Vec<String> = stream::iter(items.clone())
                .map(|r: ObjRef| async move {
                    get_prop::<String>(conn, &r, I_ACC, "Name")
                        .await
                        .unwrap_or_default()
                })
                .buffered(CONCURRENCY)
                .collect()
                .await;
            let exact: Vec<usize> = names
                .iter()
                .enumerate()
                .filter(|(_, n)| **n == want)
                .map(|(i, _)| i)
                .collect();
            let pick = if exact.len() == 1 {
                exact[0]
            } else if exact.is_empty() {
                let ci: Vec<usize> = names
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| n.trim().eq_ignore_ascii_case(want.trim()))
                    .map(|(i, _)| i)
                    .collect();
                match ci.len() {
                    1 => ci[0],
                    0 => bail!(
                        "no option named {} (options: {})",
                        quote(&want, 60),
                        names
                            .iter()
                            .take(20)
                            .map(|n| quote(n, 40))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    _ => bail!("option name {} is ambiguous", quote(&want, 60)),
                }
            } else {
                bail!(
                    "option name {} is ambiguous ({} matches)",
                    quote(&want, 60),
                    exact.len()
                )
            };
            let ok: bool = call_t(
                conn,
                &el.r,
                I_SELECTION,
                "SelectChild",
                &(pick as i32,),
                ACTION_TIMEOUT,
            )
            .await?;
            if !ok {
                bail!("application rejected selecting {}", quote(&want, 60));
            }
            return Ok(());
        }

        bail!(
            "element {idx} ({}) is not settable via accessibility (no EditableText, Value, Selection or toggle)",
            role_name(el.role)
        )
    }

    /// Target for text insertion: explicit `element_index`; else the object
    /// that last reported gaining focus, if it is editable text inside the
    /// window; else the single focused editable element of the latest
    /// observation (state checked live).
    async fn focused_editable(
        &mut self,
        w: &WindowInfo,
        element_index: Option<u32>,
    ) -> Result<Elem> {
        if let Some(idx) = element_index {
            let el = self.resolve(w, idx).await?;
            if !el.has_iface(I_EDIT) {
                bail!(
                    "element {idx} ({}) is not editable text",
                    role_name(el.role)
                );
            }
            require_enabled(&el, idx)?;
            require_editable(&el, idx)?;
            return Ok(el);
        }
        let known = {
            let ws = self
                .obs
                .windows
                .get(&w.window_id)
                .filter(|ws| ws.valid && ws.pid == w.pid)
                .ok_or_else(|| {
                    anyhow!(
                        "the accessibility tree of window {} is unavailable",
                        w.window_id
                    )
                })?;
            let focus_event = ws.frame.as_ref().and_then(|f| {
                self.events
                    .lock()
                    .ok()
                    .and_then(|l| l.focused.get(&f.bus).cloned())
            });
            focus_event.and_then(|r| ws.elements.iter().find(|(_, e)| e.r == r).map(|(i, _)| *i))
        };
        if let Some(idx) = known {
            if let Ok(el) = self.resolve(w, idx).await {
                if el.has_iface(I_EDIT)
                    && has(el.states, S_FOCUSED)
                    && has(el.states, S_EDITABLE)
                    && !has(el.states, S_READ_ONLY)
                {
                    return Ok(el);
                }
            }
        }
        let ws = self.obs.windows.get(&w.window_id).expect("checked above");
        let editable: Vec<(u32, ObjRef)> = ws
            .elements
            .iter()
            .filter(|(_, e)| e.has_iface(I_EDIT))
            .map(|(i, e)| (*i, e.r.clone()))
            .collect();
        let conn = &self.conn;
        let focused: Vec<u32> = stream::iter(editable)
            .map(|(i, r)| async move {
                (
                    i,
                    live_states(conn, &r)
                        .await
                        .map(|s| has(s, S_FOCUSED))
                        .unwrap_or(false),
                )
            })
            .buffer_unordered(CONCURRENCY)
            .filter_map(|(i, f)| async move { f.then_some(i) })
            .collect()
            .await;
        match focused.as_slice() {
            [idx] => {
                let idx = *idx;
                let el = self.resolve(w, idx).await?;
                require_enabled(&el, idx)?;
                require_editable(&el, idx)?;
                Ok(el)
            }
            [] => bail!("no focused editable text element in the latest observation"),
            many => bail!(
                "{} editable elements report focus; pass element_index",
                many.len()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tree rendering
// ---------------------------------------------------------------------------

/// Depth-first walk in document order. Skips hidden subtrees and subtrees
/// entirely outside the window (except popups, which may extend beyond it).
/// `visit` returns whether the node was emitted (affects indentation).
fn walk(
    tree: &Tree,
    rects: &HashMap<ObjRef, Rect>,
    frame: &ObjRef,
    ww: i32,
    wh: i32,
    offscreen: &mut usize,
    visit: &mut dyn FnMut(&ObjRef, &Node, usize) -> bool,
) {
    let mut stack: Vec<(ObjRef, usize, bool, usize)> = vec![(frame.clone(), 0, false, 0)];
    while let Some((r, depth, in_popup, level)) = stack.pop() {
        if level > MAX_DEPTH {
            continue;
        }
        let Some(n) = tree.nodes.get(&r) else {
            continue;
        };
        let is_root = &r == frame;
        if !is_root && (!has(n.states, S_SHOWING) || has(n.states, S_DEFUNCT)) {
            continue;
        }
        let popup = in_popup || (!is_root && is_popup_role(n.role));
        if !is_root && !popup {
            if let Some(rc) = rects.get(&r) {
                if rc.w > 0 && rc.h > 0 && rc.outside(ww, wh) {
                    *offscreen += 1;
                    continue;
                }
            }
        }
        let emitted = visit(&r, n, depth);
        let child_depth = depth + emitted as usize;
        if let Some(kids) = tree.kids.get(&r) {
            for k in kids.iter().rev() {
                stack.push((k.clone(), child_depth, popup, level + 1));
            }
        }
    }
}

fn interesting(n: &Node, d: Option<&Details>) -> bool {
    if !n.name.trim().is_empty() || !n.desc.trim().is_empty() {
        return true;
    }
    if n.has_iface(I_ACTION) || n.has_iface(I_EDIT) || n.has_iface(I_VALUE) {
        return true;
    }
    if has(n.states, S_FOCUSABLE) || has(n.states, S_FOCUSED) {
        return true;
    }
    if n.has_iface(I_SELECTION) && !is_structural_role(n.role) {
        return true;
    }
    if n.has_iface(I_TEXT) {
        // Before details are known, keep text objects as candidates; afterwards
        // only if they actually carry visible text.
        return match d {
            None => true,
            Some(d) => d
                .text
                .as_ref()
                .map(|(t, _)| !t.trim().is_empty())
                .unwrap_or(false),
        };
    }
    false
}

/// Text, value and actions of an object. `actions` are the names already
/// known from an earlier observation (they do not change for a widget).
async fn fetch_details(
    conn: &Connection,
    r: &ObjRef,
    n: &Node,
    actions: Option<Vec<String>>,
) -> Details {
    let mut d = Details {
        actions,
        ..Details::default()
    };
    if n.has_iface(I_TEXT) && n.role != R_PASSWORD_TEXT {
        if let Ok(count) = get_prop::<i32>(conn, r, I_TEXT, "CharacterCount").await {
            if count > 0 {
                let end = count.min(MAX_TEXT_CHARS);
                if let Ok(t) = call::<_, String>(conn, r, I_TEXT, "GetText", &(0i32, end)).await {
                    // U+FFFC marks embedded child objects in hypertext.
                    let t: String = t.chars().filter(|c| *c != '\u{fffc}').collect();
                    d.text = Some((t, count));
                }
            } else {
                d.text = Some((String::new(), 0));
            }
        }
    }
    if n.has_iface(I_VALUE) {
        if let Ok(props) =
            call::<_, HashMap<String, OwnedValue>>(conn, r, PROPS, "GetAll", &(I_VALUE,)).await
        {
            let cur = pv::<f64>(&props, "CurrentValue");
            let lo = pv::<f64>(&props, "MinimumValue");
            let hi = pv::<f64>(&props, "MaximumValue");
            let txt = pv::<String>(&props, "Text").filter(|s| !s.is_empty());
            if let Some(c) = cur {
                let mut s = txt.map(|t| quote(&t, 40)).unwrap_or_else(|| fmt_num(c));
                if let (Some(lo), Some(hi)) = (lo, hi) {
                    s.push_str(&format!(" ({}..{})", fmt_num(lo), fmt_num(hi)));
                }
                d.value = Some(s);
            }
        }
    }
    if n.has_iface(I_ACTION) && d.actions.is_none() {
        if let Ok(a) =
            call::<_, Vec<(String, String, String)>>(conn, r, I_ACTION, "GetActions", &()).await
        {
            d.actions = Some(
                a.into_iter()
                    .map(|(n, _, _)| n)
                    .filter(|n| !n.is_empty())
                    .collect(),
            );
        }
    }
    d
}

fn render_line(n: &Node, d: Option<&Details>, rect: Option<&Rect>, ww: i32, wh: i32) -> String {
    let mut s = role_name(n.role);
    let name = n.name.trim();
    if !name.is_empty() {
        s.push(' ');
        s.push_str(&quote(name, MAX_NAME_CHARS));
    }
    let st = n.states;
    let mut flags: Vec<&str> = Vec::new();
    if has(st, S_FOCUSED) {
        flags.push("focused");
    }
    if has(st, S_ACTIVE) && is_toplevel_role(n.role) {
        flags.push("active");
    }
    if (n.has_iface(I_ACTION) || n.has_iface(I_EDIT) || has(st, S_FOCUSABLE))
        && (!has(st, S_ENABLED) && !has(st, S_SENSITIVE))
    {
        flags.push("disabled");
    }
    if has(st, S_SELECTED) {
        flags.push("selected");
    }
    if has(st, S_INDETERMINATE) {
        flags.push("mixed");
    } else if has(st, S_CHECKED) {
        flags.push("checked");
    } else if has(st, S_CHECKABLE) || is_toggle_role(n.role) {
        flags.push("unchecked");
    }
    if has(st, S_PRESSED) {
        flags.push("pressed");
    }
    if has(st, S_EXPANDED) {
        flags.push("expanded");
    } else if has(st, S_EXPANDABLE) || has(st, S_COLLAPSED) {
        flags.push("collapsed");
    }
    if n.has_iface(I_EDIT) {
        if has(st, S_READ_ONLY) || !has(st, S_EDITABLE) {
            flags.push("readonly");
        } else {
            flags.push("editable");
        }
    }
    if has(st, S_MODAL) {
        flags.push("modal");
    }
    if has(st, S_REQUIRED) {
        flags.push("required");
    }
    if has(st, S_INVALID_ENTRY) {
        flags.push("invalid");
    }
    if has(st, S_BUSY) {
        flags.push("busy");
    }
    if !flags.is_empty() {
        s.push_str(&format!(" ({})", flags.join(", ")));
    }
    let desc = n.desc.trim();
    if !desc.is_empty() && desc != name {
        s.push_str(&format!(" desc={}", quote(desc, 120)));
    }
    if let Some(d) = d {
        if let Some(v) = &d.value {
            s.push_str(&format!(" value={v}"));
        }
        if let Some((t, count)) = &d.text {
            let t = t.trim();
            if !t.is_empty() && t != name {
                s.push_str(&format!(" text={}", quote(t, MAX_TEXT_CHARS as usize)));
                if *count > MAX_TEXT_CHARS {
                    s.push_str(&format!(" ({count} chars)"));
                }
            }
        }
    }
    if n.role == R_PASSWORD_TEXT {
        s.push_str(" text=<hidden>");
    }
    if let Some(r) = rect {
        s.push_str(&format!(" @{},{} {}x{}", r.x, r.y, r.w, r.h));
        if r.w > 0 && r.h > 0 && r.outside(ww, wh) {
            s.push_str(" (outside window)");
        }
    }
    if let Some(actions) = d.and_then(|d| d.actions.as_ref()) {
        if !actions.is_empty() {
            let shown: Vec<&str> = actions
                .iter()
                .take(MAX_ACTIONS_SHOWN)
                .map(String::as_str)
                .collect();
            s.push_str(&format!(" actions=[{}]", shown.join(", ")));
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Action helpers
// ---------------------------------------------------------------------------

fn require_enabled(el: &Elem, idx: u32) -> Result<()> {
    if !has(el.states, S_ENABLED) && !has(el.states, S_SENSITIVE) {
        bail!("element {idx} is disabled");
    }
    Ok(())
}

fn require_editable(el: &Elem, idx: u32) -> Result<()> {
    if has(el.states, S_READ_ONLY) || !has(el.states, S_EDITABLE) {
        bail!("element {idx} is read-only");
    }
    Ok(())
}

/// (non-localized name, localized name) per action index.
async fn action_names(conn: &Connection, el: &Elem) -> Result<Vec<(String, String)>> {
    if !el.has_iface(I_ACTION) {
        return Ok(Vec::new());
    }
    let localized: Vec<(String, String, String)> =
        call(conn, &el.r, I_ACTION, "GetActions", &()).await?;
    let mut out = Vec::new();
    for (i, (loc, _, _)) in localized.into_iter().enumerate().take(32) {
        let name = call::<_, String>(conn, &el.r, I_ACTION, "GetName", &(i as i32,))
            .await
            .unwrap_or_else(|_| loc.clone());
        out.push((name, loc));
    }
    Ok(out)
}

const CLICK_ACTIONS: [&str; 7] = [
    "click", "press", "activate", "toggle", "jump", "open", "select",
];
const TOGGLE_ACTIONS: [&str; 4] = ["toggle", "click", "press", "activate"];

/// Index of the first action named like one of `wanted` (in preference
/// order). Never an arbitrary other action.
fn preferred_action(acts: &[(String, String)], wanted: &[&str]) -> Option<usize> {
    wanted
        .iter()
        .find_map(|want| acts.iter().position(|(n, _)| n.eq_ignore_ascii_case(want)))
}

fn list_actions(acts: &[(String, String)]) -> String {
    if acts.is_empty() {
        return "none".into();
    }
    acts.iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Hint for a showing directory pane without any item objects (icon views
/// of GTK file managers draw files without accessible children).
fn empty_item_view_hint(tree: &Tree) -> Option<String> {
    let item = |n: &Node| matches!(n.role, R_TABLE_CELL | R_LIST_ITEM | R_ICON | R_TREE_ITEM);
    let pane = tree
        .nodes
        .iter()
        .find(|(_, n)| n.role == R_DIRECTORY_PANE && has(n.states, S_SHOWING))?;
    let inside = |n: &Node| {
        let mut cur = n.parent.clone();
        for _ in 0..MAX_DEPTH {
            match cur {
                Some(r) if &r == pane.0 => return true,
                Some(r) => cur = tree.nodes.get(&r).and_then(|n| n.parent.clone()),
                None => return false,
            }
        }
        false
    };
    let has_items = tree.nodes.values().any(|n| item(n) && inside(n));
    (!has_items).then(|| format!(
        "the {} {} exposes no file items to accessibility (an icon view); a list view (in Thunar and Nautilus: Ctrl+2) lists each file as an element you can click, rename (F2) and cut/paste",
        role_name(pane.1.role),
        quote(&pane.1.name, 40)
    ))
}

/// Where an element can be clicked: the screen centre of the part of it
/// that its ancestors leave visible (scroll panes and viewports clip their
/// children), the element's own extents, and the objects a hit test starts
/// from and passes through.
struct ClickGeometry {
    center: (i32, i32),
    rect: Rect,
    /// Ancestors up to and including `top` (parent first).
    ancestors: Vec<ObjRef>,
    /// Where the accessible hit test starts: the top-level, or for a menu
    /// item its menu.
    start: ObjRef,
    top_role: Option<u32>,
}

/// Menu items are not clipped by their parent menu, whose extents are those
/// of its menu-bar entry rather than of the popup; their hit test starts at
/// that menu, which reports its items at points on the popup.
async fn click_geometry(
    conn: &Connection,
    el: &Elem,
    menu_item: bool,
) -> Result<Option<ClickGeometry>> {
    let Some(rect) = extents(conn, &el.r, COORD_SCREEN)
        .await?
        .filter(|r| r.w > 0 && r.h > 0)
    else {
        return Ok(None);
    };
    let mut clip = rect;
    let mut ancestors = Vec::new();
    let mut top_role = None;
    let mut cur = el.r.clone();
    for _ in 0..32 {
        let parent: (String, OwnedObjectPath) = get_prop(conn, &cur, I_ACC, "Parent").await?;
        let p = ObjRef::from_wire(&parent);
        if p.is_null() {
            break;
        }
        let role: u32 = call(conn, &p, I_ACC, "GetRole", &()).await?;
        if role == R_APPLICATION {
            break;
        }
        ancestors.push(p.clone());
        if !menu_item {
            if let Ok(Some(pr)) = extents(conn, &p, COORD_SCREEN).await {
                if pr.w > 0 && pr.h > 0 {
                    match clip.intersect(&pr) {
                        Some(c) => clip = c,
                        None => return Ok(None),
                    }
                }
            }
        }
        if is_toplevel_role(role) {
            top_role = Some(role);
            break;
        }
        cur = p;
    }
    let start = if menu_item {
        ancestors.first()
    } else {
        ancestors.last()
    };
    let Some(start) = start.cloned() else {
        return Ok(None);
    };
    Ok(Some(ClickGeometry {
        center: (clip.x + clip.w / 2, clip.y + clip.h / 2),
        rect,
        ancestors,
        start,
        top_role,
    }))
}

/// What the toolkit reports at a screen point, descending from `g.start`
/// with Component.GetAccessibleAtPoint.
#[derive(Debug, PartialEq)]
enum PointHit {
    /// The element or one of its descendants.
    Element,
    /// Another element covers the point (its description).
    Other(String),
    /// The toolkit did not answer down to the element.
    Unknown,
}

async fn hit_at_point(conn: &Connection, target: &ObjRef, g: &ClickGeometry) -> PointHit {
    let (x, y) = g.center;
    let ancestors: std::collections::HashSet<&ObjRef> = g.ancestors.iter().collect();
    let parent = g.ancestors.first();
    let mut cur = g.start.clone();
    let trace = |what: &str, detail: String| {
        if crate::trace_on() {
            eprintln!("trace hit_at_point {what} {detail}");
        }
    };
    for _ in 0..MAX_DEPTH {
        let r: Result<(String, OwnedObjectPath)> = call(
            conn,
            &cur,
            I_COMPONENT,
            "GetAccessibleAtPoint",
            &(x, y, COORD_SCREEN),
        )
        .await;
        let r = match r {
            Ok(r) => ObjRef::from_wire(&r),
            Err(e) => {
                trace("call", format!("{} failed: {e:#}", cur.path));
                return PointHit::Unknown;
            }
        };
        if r.is_null() || r == cur {
            // The toolkit stops at a container above the element (GTK's
            // scrolled window answers null for the rows of a tree view). An
            // element of a table is then resolved by its row geometry.
            trace(
                "stop",
                format!(
                    "at {} (null={}) parent={:?}",
                    cur.path,
                    r.is_null(),
                    parent.map(|p| &p.path)
                ),
            );
            return table_hit(conn, target, &g.ancestors, (x, y)).await;
        }
        if &r == target {
            return PointHit::Element;
        }
        if !ancestors.contains(&r) {
            // A descendant of the element (its label), or something else.
            let mut up = r.clone();
            let mut hit_parent = None;
            for step in 0..MAX_DEPTH {
                let Ok(parent) =
                    get_prop::<(String, OwnedObjectPath)>(conn, &up, I_ACC, "Parent").await
                else {
                    break;
                };
                let p = ObjRef::from_wire(&parent);
                if step == 0 {
                    hit_parent = Some(p.clone());
                }
                if &p == target {
                    return PointHit::Element;
                }
                if p.is_null() || ancestors.contains(&p) {
                    break;
                }
                up = p;
            }
            // A sibling of the element is the toolkit's imprecision, not a
            // cover: GTK tree views answer with the neighbouring cell (their
            // hit test leaves out the header row). A table resolves it by
            // the geometry of its rows; anything else does not stop the click.
            if hit_parent.is_some() && hit_parent.as_ref() == parent {
                return table_hit(conn, target, &g.ancestors, (x, y)).await;
            }
            let role: u32 = call(conn, &r, I_ACC, "GetRole", &()).await.unwrap_or(0);
            let name: String = get_prop(conn, &r, I_ACC, "Name").await.unwrap_or_default();
            return PointHit::Other(format!("{} {}", role_name(role), quote(&name, 40)));
        }
        trace("descend", format!("{} -> {}", cur.path, r.path));
        cur = r;
    }
    trace("depth", "exhausted".into());
    PointHit::Unknown
}

/// Hit test inside a table by the geometry of its cells, for toolkits whose
/// `GetAccessibleAtPoint` is imprecise there: the row at the point's height
/// is found by binary search over the rows of the element's column (rows
/// have increasing y), using the Table interface of the nearest ancestor
/// that implements it (GTK's multi-renderer cells sit inside a container
/// cell). `Unknown` when no near ancestor is a table or a row has no
/// geometry.
async fn table_hit(
    conn: &Connection,
    target: &ObjRef,
    ancestors: &[ObjRef],
    (_, y): (i32, i32),
) -> PointHit {
    let trace = |what: &str, detail: String| {
        if crate::trace_on() {
            eprintln!("trace table_hit {what} {detail}");
        }
    };
    let mut child = target;
    let mut found = None;
    for table in ancestors.iter().take(4) {
        let idx = match call::<_, i32>(conn, child, I_ACC, "GetIndexInParent", &()).await {
            Ok(i) => i,
            Err(e) => {
                trace("index", format!("{e:#}"));
                return PointHit::Unknown;
            }
        };
        match (
            call::<_, i32>(conn, table, I_TABLE, "GetRowAtIndex", &(idx,)).await,
            call::<_, i32>(conn, table, I_TABLE, "GetColumnAtIndex", &(idx,)).await,
        ) {
            (Ok(row), Ok(col)) => {
                found = Some((table, row, col));
                break;
            }
            // Not a table (or not answering): try the next ancestor.
            (r, c) => {
                trace(
                    "row/col",
                    format!("{}: {:?} {:?}", table.path, r.err(), c.err()),
                );
                child = table;
            }
        }
    }
    let Some((table, row, col)) = found else {
        trace("table", "none among the near ancestors".into());
        return PointHit::Unknown;
    };
    let nrows = match get_prop::<i32>(conn, table, I_TABLE, "NRows").await {
        Ok(n) => n,
        Err(e) => {
            trace("nrows", format!("{e:#}"));
            return PointHit::Unknown;
        }
    };
    trace(
        "table",
        format!("{} row={row} col={col} nrows={nrows} y={y}", table.path),
    );
    if row < 0 || col < 0 || nrows <= 0 {
        return PointHit::Unknown;
    }
    let (mut lo, mut hi) = (0i32, nrows - 1);
    for _ in 0..24 {
        if lo > hi {
            break;
        }
        let mid = lo + (hi - lo) / 2;
        let Ok(cell) =
            call::<_, WireRef>(conn, table, I_TABLE, "GetAccessibleAt", &(mid, col)).await
        else {
            return PointHit::Unknown;
        };
        let cell = ObjRef::from_wire(&cell);
        let e = match extents(conn, &cell, COORD_SCREEN).await {
            Ok(Some(e)) if e.h > 0 => e,
            other => {
                trace("extents", format!("row {mid}: {other:?}"));
                return PointHit::Unknown;
            }
        };
        if y < e.y {
            hi = mid - 1;
        } else if y >= e.y + e.h {
            lo = mid + 1;
        } else if mid == row {
            return PointHit::Element;
        } else {
            let name: String = get_prop(conn, &cell, I_ACC, "Name")
                .await
                .unwrap_or_default();
            return PointHit::Other(format!(
                "table cell {} in row {mid} (the element is in row {row})",
                quote(&name, 40)
            ));
        }
    }
    PointHit::Unknown
}

async fn parent_role(conn: &Connection, r: &ObjRef) -> Option<u32> {
    let parent: (String, OwnedObjectPath) = get_prop(conn, r, I_ACC, "Parent").await.ok()?;
    let p = ObjRef::from_wire(&parent);
    if p.is_null() {
        return None;
    }
    call(conn, &p, I_ACC, "GetRole", &()).await.ok()
}

async fn do_action(conn: &Connection, el: &Elem, i: i32) -> Result<()> {
    let ok: bool = call_t(conn, &el.r, I_ACTION, "DoAction", &(i,), ACTION_TIMEOUT)
        .await
        .context("DoAction failed or timed out; the outcome is unknown")?;
    if !ok {
        bail!("application rejected action #{i}");
    }
    Ok(())
}

async fn select_in_parent(conn: &Connection, el: &Elem) -> Result<()> {
    let parent: (String, OwnedObjectPath) = get_prop(conn, &el.r, I_ACC, "Parent").await?;
    let parent = ObjRef::from_wire(&parent);
    if parent.is_null() {
        bail!("element has no parent to select within");
    }
    let ifaces: Vec<String> = call(conn, &parent, I_ACC, "GetInterfaces", &()).await?;
    if !ifaces.iter().any(|i| i == I_SELECTION) {
        bail!("parent does not support selection");
    }
    let idx: i32 = call(conn, &el.r, I_ACC, "GetIndexInParent", &()).await?;
    if idx < 0 {
        bail!("element has no index in its parent");
    }
    let ok: bool = call_t(
        conn,
        &parent,
        I_SELECTION,
        "SelectChild",
        &(idx,),
        ACTION_TIMEOUT,
    )
    .await?;
    if !ok {
        bail!("application rejected selection");
    }
    Ok(())
}

fn parse_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().and_then(|i| match i {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "on" | "checked" | "yes" | "1" | "selected" => Some(true),
            "false" | "off" | "unchecked" | "no" | "0" | "unselected" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Inserts at the caret (replacing a selection). Returns a warning when the
/// field afterwards does not contain the text as given (the application
/// reformatted or rejected part of it).
async fn insert_text(conn: &Connection, el: &Elem, text: &str) -> Result<Option<String>> {
    if !el.has_iface(I_EDIT) || !el.has_iface(I_TEXT) {
        bail!("target is not editable text");
    }
    if text.is_empty() {
        return Ok(None);
    }
    if text.len() > MAX_INSERT_BYTES {
        bail!("text too large ({} bytes)", text.len());
    }
    if text.contains('\n') && has(el.states, S_SINGLE_LINE) {
        bail!("newline cannot be inserted into a single-line field semantically; use press_key Return");
    }
    let caret: i32 = get_prop(conn, &el.r, I_TEXT, "CaretOffset").await?;
    let mut pos = if caret >= 0 {
        caret
    } else {
        get_prop::<i32>(conn, &el.r, I_TEXT, "CharacterCount").await?
    };
    let nsel: i32 = call(conn, &el.r, I_TEXT, "GetNSelections", &())
        .await
        .unwrap_or(0);
    if nsel > 0 {
        let (s, e): (i32, i32) = call(conn, &el.r, I_TEXT, "GetSelection", &(0i32,)).await?;
        if e > s && s >= 0 {
            let ok: bool =
                call_t(conn, &el.r, I_EDIT, "DeleteText", &(s, e), ACTION_TIMEOUT).await?;
            if !ok {
                bail!("application refused to replace the selected text");
            }
            pos = s;
        }
    }
    // Per EditableText.xml the length argument is a byte count.
    let ok: bool = call_t(
        conn,
        &el.r,
        I_EDIT,
        "InsertText",
        &(pos, text, text.len() as i32),
        ACTION_TIMEOUT,
    )
    .await?;
    if !ok {
        bail!("application rejected InsertText");
    }
    let n = text.chars().count() as i32;
    let got: String = call(
        conn,
        &el.r,
        I_TEXT,
        "GetText",
        &(pos, pos.saturating_add(n)),
    )
    .await?;
    if got != text {
        let count: i32 = get_prop(conn, &el.r, I_TEXT, "CharacterCount")
            .await
            .unwrap_or(-1);
        let all: String = if count >= 0 {
            call(conn, &el.r, I_TEXT, "GetText", &(0i32, count.min(400)))
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };
        return Ok(Some(format!(
            "the application changed the inserted text: typed {}, the field now contains {}",
            quote(text, 80),
            quote(&all, 120)
        )));
    }
    let _: Result<bool> = call(
        conn,
        &el.r,
        I_TEXT,
        "SetCaretOffset",
        &(pos.saturating_add(n),),
    )
    .await;
    Ok(None)
}

/// Locales whose decimal separator is a comma (LC_NUMERIC language part).
const COMMA_DECIMAL: [&str; 30] = [
    "de", "fr", "es", "it", "nl", "pt", "ru", "pl", "cs", "sk", "da", "fi", "sv", "nb", "nn", "no",
    "tr", "el", "hu", "ro", "bg", "uk", "hr", "sl", "sr", "lt", "lv", "et", "id", "vi",
];

/// Hint for a reformatted insertion of a number with '.' decimals in an app
/// whose locale (from its environment) uses a decimal comma.
pub fn locale_hint(pid: u32, text: &str) -> Option<String> {
    decimal_hint(&numeric_locale(pid)?, text)
}

/// The locale a process formats numbers with (LC_ALL, LC_NUMERIC, LANG).
fn numeric_locale(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let var = |k: &str| {
        env.split(|&b| b == 0).find_map(|e| {
            let e = String::from_utf8_lossy(e);
            e.strip_prefix(&format!("{k}="))
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        })
    };
    var("LC_ALL")
        .or_else(|| var("LC_NUMERIC"))
        .or_else(|| var("LANG"))
}

fn decimal_hint(locale: &str, text: &str) -> Option<String> {
    let lang = locale.split(['_', '.', '@']).next()?.to_ascii_lowercase();
    if !COMMA_DECIMAL.contains(&lang.as_str()) {
        return None;
    }
    // The first number with '.' decimals in the text, as an example.
    let b = text.as_bytes();
    let dot = (1..b.len().saturating_sub(1))
        .find(|&i| b[i] == b'.' && b[i - 1].is_ascii_digit() && b[i + 1].is_ascii_digit())?;
    let start = (0..dot)
        .rev()
        .take_while(|&i| b[i].is_ascii_digit())
        .last()?;
    let end = (dot + 1..b.len())
        .take_while(|&i| b[i].is_ascii_digit())
        .last()?
        + 1;
    let number = &text[start..end];
    Some(format!(
        "the app formats numbers for locale {locale}, whose decimal separator is a comma: type {} rather than {number}",
        number.replace('.', ",")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_owner_default_buses() {
        let chk = |a: &str| check_private_address(a, "x", false);
        assert!(chk("unix:path=/run/user/1000/bus").is_err());
        assert!(chk("unix:path=/run/user/1000/at-spi/bus_0").is_err());
        assert!(chk("unix:path=/run/user/1000/at-spi/bus").is_err());
        assert!(chk("autolaunch:").is_err());
        assert!(chk("").is_err());
        assert!(chk("unix:path=/tmp/cua-7/bus").is_ok());
        assert!(chk("unix:path=/tmp/cua/at-spi/bus_9,guid=abc").is_ok());
        // Literal-string comparison used to miss these spellings.
        assert!(chk("unix:path=/run/user/1000//bus").is_err());
        assert!(chk("unix:path=/run/user/1000/./at-spi/../bus").is_err());
        assert!(chk("unix:path=/tmp/../run/user/1000/bus").is_err());
        assert!(chk("unix:path=%2frun%2fuser/1000/bus").is_err());
        assert!(
            chk("unix:path=/var/run/user/1000/bus").is_err()
                || !std::path::Path::new("/var/run").exists()
        );
        assert!(chk("unix:path=/tmp/private/bus;unix:path=/run/user/1000/bus").is_err());
        // Implicit locations and unverifiable sockets.
        assert!(chk("unix:runtime=yes").is_err());
        assert!(chk("unix:tmpdir=/tmp").is_err());
        assert!(chk("tcp:host=localhost,port=4000").is_err());
        assert!(chk("unix:abstract=/tmp/dbus-x").is_err());
        assert!(check_private_address("unix:abstract=/tmp/dbus-x", "x", true).is_ok());
        assert!(chk("unix:path=%zz").is_err());
    }

    #[allow(dead_code)]
    fn futures_are_send(a: &mut Accessibility, w: &WindowInfo, p: &Value) {
        fn is_send<T: Send>(_: T) {}
        is_send(Accessibility::connect(1));
        is_send(a.snapshot(w));
        is_send(a.action(w, "click", p));
    }

    #[test]
    fn role_and_state_tables() {
        assert_eq!(role_name(R_FRAME), "frame");
        assert_eq!(role_name(43), "button");
        assert_eq!(role_name(R_SWITCH), "switch");
        assert_eq!(role_name(999), "role#999");
        let s = states_from_wire(&[1 << S_SHOWING, 1 << (S_READ_ONLY - 32)]);
        assert!(has(s, S_SHOWING) && has(s, S_READ_ONLY) && !has(s, S_FOCUSED));
    }

    #[test]
    fn ids_are_stable_and_role_bound() {
        let mut obs = Observations::default();
        obs.begin(&window(9, 1));
        let a = ObjRef::new(":1.5", "/a");
        let b = ObjRef::new(":1.5", "/b");
        let ia = obs.id_for(9, &a, 43);
        let ib = obs.id_for(9, &b, 43);
        assert_ne!(ia, ib);
        assert_eq!(obs.id_for(9, &a, 43), ia);
        let ia2 = obs.id_for(9, &a, 29);
        assert!(ia2 != ia && ia2 != ib);
    }

    fn window(window_id: u32, pid: u32) -> WindowInfo {
        WindowInfo {
            window_id,
            title: "W".into(),
            app_id: "app".into(),
            pid,
            width: 100,
            height: 100,
            transient_for: None,
            origin: (0, 0),
        }
    }

    /// Regression: window state is replaced on turn reset, owner change and
    /// frame remap; IDs issued earlier (e.g. 7 = Cancel) must never be issued
    /// again for another element (e.g. Delete), or a stale ID would pass
    /// revalidation against the new element.
    #[test]
    fn element_ids_are_never_reused_after_state_replacement() {
        let mut obs = Observations::default();
        let w = window(9, 1);
        let mut issued = Vec::new();
        let mut next = |obs: &mut Observations, path: &str| {
            let id = obs.id_for(9, &ObjRef::new(":1.5", path), 43);
            issued.push(id);
        };
        obs.begin(&w);
        obs.bind_frame(9, &ObjRef::new(":1.5", "/frame"));
        next(&mut obs, "/cancel");
        obs.reset(); // turn_ended
        obs.begin(&w);
        next(&mut obs, "/delete");
        obs.begin(&window(9, 2)); // same XID, new owner
        next(&mut obs, "/other");
        obs.begin(&window(9, 2));
        obs.bind_frame(9, &ObjRef::new(":1.5", "/frame"));
        assert!(obs.bind_frame(9, &ObjRef::new(":1.5", "/frame2"))); // remap
        next(&mut obs, "/remapped");
        obs.begin(&window(10, 3)); // another window
        let other = obs.id_for(10, &ObjRef::new(":1.6", "/x"), 43);
        issued.push(other);
        let unique: std::collections::HashSet<_> = issued.iter().collect();
        assert_eq!(unique.len(), issued.len(), "IDs reused: {issued:?}");
    }

    /// Regression: starting a snapshot cleared the element map, so a
    /// snapshot that then ran out of budget (large web pages) left no element
    /// usable. Only a successful snapshot replaces the map.
    #[test]
    fn element_map_survives_a_started_snapshot() {
        let mut obs = Observations::default();
        let w = window(9, 1);
        obs.begin(&w);
        obs.bind_frame(9, &ObjRef::new(":1.5", "/frame"));
        let r = ObjRef::new(":1.5", "/ok");
        let id = obs.id_for(9, &r, 43);
        let ws = obs.windows.get_mut(&9).unwrap();
        ws.elements.insert(
            id,
            Elem {
                r,
                role: 43,
                name: "OK".into(),
                ifaces: vec![],
                states: 0,
            },
        );
        ws.valid = true;
        obs.begin(&w); // a new snapshot starts (and may fail)
        let ws = &obs.windows[&9];
        assert!(ws.valid && ws.elements.contains_key(&id));
        obs.begin(&window(9, 2)); // a new owner still starts over
        assert!(obs.windows[&9].elements.is_empty());
    }

    #[test]
    fn hint_for_item_views_without_accessible_items() {
        let node = |role: u32, parent: Option<&ObjRef>| Node {
            parent: parent.cloned(),
            child_count: 0,
            ifaces: vec![],
            name: "Icon view".into(),
            role,
            desc: String::new(),
            states: 1 << S_SHOWING,
        };
        let (frame, pane, cell) = (
            ObjRef::new(":1.2", "/f"),
            ObjRef::new(":1.2", "/p"),
            ObjRef::new(":1.2", "/c"),
        );
        let mut tree = Tree::default();
        tree.nodes.insert(frame.clone(), node(R_FRAME, None));
        tree.nodes
            .insert(pane.clone(), node(R_DIRECTORY_PANE, Some(&frame)));
        // A sidebar cell outside the pane does not count.
        tree.nodes.insert(
            ObjRef::new(":1.2", "/side"),
            node(R_TABLE_CELL, Some(&frame)),
        );
        assert!(empty_item_view_hint(&tree).unwrap().contains("Ctrl+2"));
        tree.nodes.insert(cell, node(R_TABLE_CELL, Some(&pane)));
        assert!(empty_item_view_hint(&tree).is_none());
    }

    #[test]
    fn click_never_falls_back_to_an_arbitrary_action() {
        let acts = |names: &[&str]| -> Vec<(String, String)> {
            names
                .iter()
                .map(|n| (n.to_string(), n.to_string()))
                .collect()
        };
        assert_eq!(
            preferred_action(&acts(&["delete", "edit"]), &CLICK_ACTIONS),
            None
        );
        assert_eq!(
            preferred_action(&acts(&["delete", "Press"]), &CLICK_ACTIONS),
            Some(1)
        );
        assert_eq!(
            preferred_action(&acts(&["press", "toggle"]), &TOGGLE_ACTIONS),
            Some(1)
        );
        assert_eq!(preferred_action(&acts(&["remove"]), &TOGGLE_ACTIONS), None);
    }

    #[test]
    fn decimal_comma_hint() {
        let h = decimal_hint("de_DE.UTF-8", "125.50+78.25").unwrap();
        assert!(h.contains("type 125,50 rather than 125.50"), "{h}");
        assert!(decimal_hint("en_US.UTF-8", "125.50").is_none());
        assert!(decimal_hint("de_DE.UTF-8", "v1.x and 3.").is_none());
        assert!(decimal_hint("fr_FR", "x=0.5").unwrap().contains("0,5"));
    }

    #[test]
    fn removal_log_prunes_only_records_no_observation_needs() {
        let mut log = EventLog::default();
        for i in 0..(LOG_PRUNE_AT as u64 + 10) {
            log.seq = i + 1;
            log.removed
                .insert(ObjRef::new(":1.1", &format!("/{i}")), i + 1);
        }
        log.floor = 100;
        log.prune();
        assert!(log.removed.values().all(|&s| s > 100));
        assert_eq!(log.removed.len(), LOG_PRUNE_AT + 10 - 100);
        assert_eq!(log.lost_before, 0);
    }

    #[test]
    fn walk_prunes_hidden_and_offscreen() {
        let frame = ObjRef::new(":1.1", "/f");
        let vis = ObjRef::new(":1.1", "/v");
        let hid = ObjRef::new(":1.1", "/h");
        let off = ObjRef::new(":1.1", "/o");
        let showing = 1u64 << S_SHOWING;
        let mut tree = Tree::default();
        for (r, st) in [
            (&frame, showing),
            (&vis, showing),
            (&hid, 0),
            (&off, showing),
        ] {
            tree.nodes.insert(
                r.clone(),
                Node {
                    name: "n".into(),
                    role: 43,
                    states: st,
                    ..Default::default()
                },
            );
        }
        tree.kids
            .insert(frame.clone(), vec![vis.clone(), hid.clone(), off.clone()]);
        let mut rects = HashMap::new();
        rects.insert(
            off.clone(),
            Rect {
                x: 500,
                y: 0,
                w: 10,
                h: 10,
            },
        );
        rects.insert(
            vis.clone(),
            Rect {
                x: 5,
                y: 5,
                w: 10,
                h: 10,
            },
        );
        let mut seen = Vec::new();
        let mut offscreen = 0;
        walk(
            &tree,
            &rects,
            &frame,
            100,
            100,
            &mut offscreen,
            &mut |r, _, d| {
                seen.push((r.path.clone(), d));
                true
            },
        );
        assert_eq!(seen, vec![("/f".to_string(), 0), ("/v".to_string(), 1)]);
        assert_eq!(offscreen, 1);
    }
}
