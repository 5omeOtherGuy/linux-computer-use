//! X11 backend: window enumeration, window-bound background input, Composite
//! capture, XI2 raw-input abort latch and XDamage-based settling.
//!
//! Clean-room implementation on top of x11rb. Design rules:
//! * Never connect to display 0, except in the explicit live-desktop mode
//!   (`LiveScope`), which only ever lists allow-listed apps.
//! * Never use XTEST, never warp the pointer, never change input focus, never
//!   grab. Input is delivered with `SendEvent` to the creating client of the
//!   target window and positioned with a per-window virtual cursor.
//! * Only windows that `list_windows` reports are valid targets.
//! * Process identity comes from the X server (X-Resource `LocalClientPID`),
//!   never from the client-writable `_NET_WM_PID` property.
//! * Physical input (anything the server reports as an XI2 raw event, which
//!   synthetic `SendEvent` input never produces) latches an abort until
//!   `clear_input_abort`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::Serialize;
use serde_json::{json, Value};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::composite::{self, ConnectionExt as _};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::res::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, ImageFormat, ImageOrder,
    KeyButMask, MapState, PropMode, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::NONE;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        _NET_CLIENT_LIST,
        _NET_WM_NAME,
        _NET_CLIENT_LIST_STACKING,
        UTF8_STRING,
        WM_STATE,
        CLIPBOARD,
        INCR,
        _LINUX_CUA_TIME,
        _LINUX_CUA_SELECTION,
    }
}

/// Window description shared with the daemon and policy layer. `pid` is the
/// X-Resource-verified PID of the client that created the window, or 0 when
/// the server cannot report it (remote client, missing extension support).
#[derive(Serialize, Clone, Debug, Default)]
pub struct WindowInfo {
    pub window_id: u32,
    pub title: String,
    pub app_id: String,
    pub pid: u32,
    pub width: u16,
    pub height: u16,
    /// WM_TRANSIENT_FOR: the window this dialog belongs to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transient_for: Option<u32>,
    /// Root coordinates of the window's origin (converts screen coordinates
    /// into the window-local ones of the protocol).
    #[serde(skip)]
    pub origin: (i32, i32),
}

/// Where a pointer point may land.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HitArea {
    /// The window area (as captured), on the window or a popup over it.
    Window,
    /// The window itself, for an element of the window: refused when one
    /// of the client's popups (a menu, a tooltip) covers the point.
    Body,
    /// One of the client's popup windows (an open menu), anywhere on screen;
    /// the element's screen rectangle (x, y, w, h) must lie on that popup,
    /// not on another one over it.
    Popup((i32, i32, i32, i32)),
}

/// Explicit live-desktop opt-in (daemon command line only). The display is
/// not checked against :0, but only windows of allow-listed apps (WM_CLASS,
/// which any client can set) whose server-verified process is one of `pids`
/// or runs one of the executables in `exes` are ever listed.
#[derive(Clone, Debug, Default)]
pub struct LiveScope {
    pub apps: HashSet<String>,
    pub pids: HashSet<u32>,
    pub exes: HashSet<std::path::PathBuf>,
}

impl LiveScope {
    fn admits(&self, w: &WindowInfo) -> bool {
        if w.pid == 0 || !self.apps.contains(&w.app_id.to_lowercase()) {
            return false;
        }
        if self.pids.contains(&w.pid) {
            return true;
        }
        !self.exes.is_empty()
            && std::fs::read_link(format!("/proc/{}/exe", w.pid))
                .is_ok_and(|exe| self.exes.contains(&exe))
    }
}

/// Outcome of the two-phase settle after an action.
#[derive(Clone, Copy, Debug)]
pub struct Settled {
    /// Damage, a popup map or an accessibility event arrived after the action.
    pub reacted: bool,
    /// The UI then stayed quiet for the quiet interval before the cap.
    pub quiet: bool,
}

/// Typed error carried inside `anyhow::Error`; the daemon can
/// `downcast_ref::<X11Error>()` to map `code` onto protocol errors.
#[derive(Debug, Clone)]
pub struct X11Error {
    pub code: &'static str,
    pub message: String,
}

impl fmt::Display for X11Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for X11Error {}

fn xerr(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(X11Error {
        code,
        message: message.into(),
    })
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    xerr("invalid_params", message)
}

const XI_ALL_MASTER_DEVICES: u16 = 1;
const SCROLL_PIXELS_PER_NOTCH: u32 = 50;
const MAX_SCROLL_NOTCHES: u32 = 100;
const DRAG_STEPS: i32 = 12;
/// Longest a button or key is held on request (`hold_ms`).
const MAX_HOLD_MS: i64 = 10_000;
/// How long a clipboard read waits for the owning app to deliver.
const CLIPBOARD_WAIT: Duration = Duration::from_millis(1500);
/// Largest clipboard text read back (bytes).
const CLIPBOARD_MAX: u32 = 4 * 1024 * 1024;
/// Bound on waiting for a client repaint after a fresh Composite redirect or
/// a resize, before the first read of its backing pixmap.
const REPAINT_WAIT: Duration = Duration::from_millis(300);
const REPAINT_QUIET: Duration = Duration::from_millis(15);
/// Granularity for noticing accessibility events while waiting on X.
const POLL_SLICE: Duration = Duration::from_millis(4);
/// Quiet interval while the app has reacted only with accessibility events
/// and shown nothing yet (it is building a dialog: GTK registers hundreds
/// of objects, with pauses, before it creates the window), in place of the
/// caller's quiet interval.
const QUIET_INVISIBLE: Duration = Duration::from_millis(150);
/// How long settling waits for a new top-level of an observed client (a
/// dialog being built) to map.
const MAP_WAIT: Duration = Duration::from_millis(1500);

// Core modifier / button masks (X11 protocol constants).
const SHIFT: u16 = 0x0001;
const CONTROL: u16 = 0x0004;
const MOD1: u16 = 0x0008;
const MOD4: u16 = 0x0040;
const MOD5: u16 = 0x0080;

// Keysyms used directly.
const XK_RETURN: u32 = 0xff0d;
const XK_TAB: u32 = 0xff09;
const XK_SHIFT_L: u32 = 0xffe1;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_META_L: u32 = 0xffe7;
const XK_META_R: u32 = 0xffe8;
const XK_ALT_L: u32 = 0xffe9;
const XK_ALT_R: u32 = 0xffea;
const XK_SUPER_L: u32 = 0xffeb;
const XK_SUPER_R: u32 = 0xffec;
const XK_ISO_LEVEL3_SHIFT: u32 = 0xfe03;
const XK_MODE_SWITCH: u32 = 0xff7e;

// ---------------------------------------------------------------------------
// Display validation
// ---------------------------------------------------------------------------

/// Extracts the display number from an X display string. Fails closed on
/// anything it cannot parse.
pub fn display_number(display: &str) -> Result<u32> {
    let s = display.trim();
    if s.is_empty() {
        return Err(xerr("display_denied", "empty display name"));
    }
    let num_part = if let Some(idx) = s.rfind("/X") {
        // Socket path form such as /tmp/.X11-unix/X3
        &s[idx + 2..]
    } else if let Some((_, after)) = s.rsplit_once(':') {
        after
    } else {
        return Err(xerr(
            "display_denied",
            format!("unparseable display name {s:?}"),
        ));
    };
    let num = num_part.split('.').next().unwrap_or("");
    num.parse::<u32>().map_err(|_| {
        xerr(
            "display_denied",
            format!("unparseable display number in {s:?}"),
        )
    })
}

fn check_display_allowed(display: &str) -> Result<()> {
    let n = display_number(display)?;
    if n == 0 {
        return Err(xerr(
            "display_denied",
            "display :0 is the owner's display and is never used",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Keyboard mapping
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

#[derive(Default)]
struct Keymap {
    /// keysym -> (keycode, modifier state that selects the keysym's level)
    lookup: HashMap<u32, (u8, u16)>,
    alt_mask: u16,
    super_mask: u16,
    level3_mask: u16,
}

impl Keymap {
    fn load(conn: &RustConnection) -> Result<Self> {
        let setup = conn.setup();
        let min = setup.min_keycode;
        let max = setup.max_keycode;
        let count = max - min + 1;
        let km = conn.get_keyboard_mapping(min, count)?.reply()?;
        let per = km.keysyms_per_keycode as usize;
        let mm = conn.get_modifier_mapping()?.reply()?;
        Ok(Self::build(min, per, &km.keysyms, &mm.keycodes))
    }

    fn build(min: u8, per: usize, syms: &[u32], modmap: &[u8]) -> Self {
        let mut km = Keymap::default();
        let sym_at = |kc: u8, idx: usize| -> u32 {
            if per == 0 || kc < min {
                return 0;
            }
            let base = (kc - min) as usize * per;
            if idx < per {
                syms.get(base + idx).copied().unwrap_or(0)
            } else {
                0
            }
        };
        let ncodes = syms.len().checked_div(per).unwrap_or(0);

        // Modifier masks for Alt, Super and Level3 from the modifier map.
        let kpm = modmap.len() / 8;
        for mod_index in 0..8usize {
            for k in 0..kpm {
                let kc = modmap[mod_index * kpm + k];
                if kc == 0 {
                    continue;
                }
                // Only Mod1..Mod5 (index 3..7) are candidates.
                if mod_index < 3 {
                    continue;
                }
                let mask = 1u16 << mod_index;
                for idx in 0..per.min(4) {
                    let slot = match sym_at(kc, idx) {
                        XK_ALT_L | XK_ALT_R | XK_META_L | XK_META_R => &mut km.alt_mask,
                        XK_SUPER_L | XK_SUPER_R => &mut km.super_mask,
                        XK_ISO_LEVEL3_SHIFT | XK_MODE_SWITCH => &mut km.level3_mask,
                        _ => continue,
                    };
                    if *slot == 0 {
                        *slot = mask;
                    }
                }
            }
        }
        if km.alt_mask == 0 {
            km.alt_mask = MOD1;
        }
        if km.super_mask == 0 {
            km.super_mask = MOD4;
        }
        if km.level3_mask == 0 {
            km.level3_mask = MOD5;
        }

        // Core keysym list layout: [G1L1, G1L2, G2L1, G2L2, G1L3, G1L4, ...].
        // Only group 1 is used; prefer the simplest level for each keysym.
        let levels: [(usize, u16); 4] = [
            (0, 0),
            (1, SHIFT),
            (4, km.level3_mask),
            (5, km.level3_mask | SHIFT),
        ];
        for (idx, state) in levels {
            for i in 0..ncodes {
                let kc = min as usize + i;
                if kc > 255 {
                    break;
                }
                let kc = kc as u8;
                let mut sym = sym_at(kc, idx);
                if idx == 1 && sym == 0 {
                    // Implicit uppercase for a lone lowercase Latin letter.
                    let base = sym_at(kc, 0);
                    if (0x61..=0x7a).contains(&base) && sym_at(kc, 1) == 0 {
                        sym = base - 0x20;
                    }
                }
                if sym != 0 {
                    km.lookup.entry(sym).or_insert((kc, state));
                }
            }
        }
        km
    }

    fn modifier(&self, m: Modifier) -> (u16, u32) {
        match m {
            Modifier::Ctrl => (CONTROL, XK_CONTROL_L),
            Modifier::Shift => (SHIFT, XK_SHIFT_L),
            Modifier::Alt => (self.alt_mask, XK_ALT_L),
            Modifier::Super => (self.super_mask, XK_SUPER_L),
        }
    }
}

/// Keysym for a character when typing text.
fn char_keysym(c: char) -> Option<u32> {
    let cp = c as u32;
    match c {
        '\n' | '\r' => Some(XK_RETURN),
        '\t' => Some(XK_TAB),
        _ if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) => Some(cp),
        _ if c.is_control() => None,
        _ => Some(0x0100_0000 | cp),
    }
}

/// Keysyms to type `text`, each on the keyboard map; nothing if any is missing.
fn keysyms_for(km: &Keymap, text: &str) -> Result<Vec<u32>> {
    let mut syms = Vec::new();
    let mut missing = Vec::new();
    for c in text.chars() {
        // Unicode keysym, else the legacy keysym keyboard maps use for it
        // (EuroSign on German layouts).
        let found = char_keysym(c)
            .into_iter()
            .chain(legacy_keysym(c))
            .find(|&s| km.lookup.contains_key(&s));
        match found {
            Some(s) => syms.push(s),
            None => {
                if !missing.contains(&c) {
                    missing.push(c)
                }
            }
        }
    }
    if !missing.is_empty() {
        return Err(xerr(
            "unsupported",
            format!(
                "cannot type {:?} as key events (not on the keyboard map) and no accessible text field has focus",
                missing.iter().take(10).collect::<String>()
            ),
        ));
    }
    Ok(syms)
}

/// Whether waiting for a dialog to close is over (see `wait_closed`):
/// `since_activity` is meaningful only when `active`.
fn close_wait_over(
    active: bool,
    since_activity: Duration,
    waited: Duration,
    idle: Duration,
    still: Duration,
    max: Duration,
) -> bool {
    waited >= max
        || if active {
            since_activity >= idle
        } else {
            waited >= still
        }
}

/// Pre-Unicode keysyms that keyboard maps still use for some characters.
fn legacy_keysym(c: char) -> Option<u32> {
    match c {
        '€' => Some(0x20ac),
        _ => None,
    }
}

fn named_keysym(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let sym = match lower.as_str() {
        "return" | "enter" => XK_RETURN,
        "kp_enter" => 0xff8d,
        "tab" => XK_TAB,
        "escape" | "esc" => 0xff1b,
        "backspace" | "back_space" => 0xff08,
        "delete" | "del" => 0xffff,
        "insert" | "ins" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "page_up" | "pageup" | "prior" | "pgup" => 0xff55,
        "page_down" | "pagedown" | "next" | "pgdn" => 0xff56,
        "left" | "arrowleft" | "arrow_left" => 0xff51,
        "up" | "arrowup" | "arrow_up" => 0xff52,
        "right" | "arrowright" | "arrow_right" => 0xff53,
        "down" | "arrowdown" | "arrow_down" => 0xff54,
        "space" | "spacebar" => 0x20,
        "capslock" | "caps_lock" => 0xffe5,
        "menu" => 0xff67,
        "print" | "printscreen" => 0xff61,
        "pause" => 0xff13,
        "plus" => 0x2b,
        "minus" => 0x2d,
        "equal" => 0x3d,
        "comma" => 0x2c,
        "period" => 0x2e,
        "slash" => 0x2f,
        "backslash" => 0x5c,
        "semicolon" => 0x3b,
        "apostrophe" => 0x27,
        "grave" => 0x60,
        "bracketleft" => 0x5b,
        "bracketright" => 0x5d,
        "ctrl" | "control" | "control_l" => XK_CONTROL_L,
        "shift" | "shift_l" => XK_SHIFT_L,
        "alt" | "alt_l" => XK_ALT_L,
        "super" | "super_l" => XK_SUPER_L,
        _ => {
            if let Some(n) = lower.strip_prefix('f') {
                if let Ok(n) = n.parse::<u32>() {
                    if (1..=35).contains(&n) {
                        return Some(0xffbe + n - 1);
                    }
                }
            }
            return None;
        }
    };
    Some(sym)
}

fn parse_modifier(token: &str) -> Result<Modifier> {
    match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "ctl" => Ok(Modifier::Ctrl),
        "shift" => Ok(Modifier::Shift),
        "alt" | "meta" | "option" => Ok(Modifier::Alt),
        "super" | "win" | "windows" | "logo" => Ok(Modifier::Super),
        "cmd" | "command" => Err(invalid(
            "modifier 'cmd' is macOS-specific; use 'ctrl' or 'super' on Linux",
        )),
        other => Err(invalid(format!("unsupported modifier {other:?}"))),
    }
}

/// Parses `ctrl+shift+t`, `Return`, `ctrl++`, `F5`, `a`.
fn parse_key_combo(spec: &str) -> Result<(Vec<Modifier>, u32)> {
    let s = spec.trim();
    if s.is_empty() {
        return Err(invalid("key must be a non-empty string"));
    }
    let (mod_part, key) = if s == "+" {
        ("", "+")
    } else if let Some(prefix) = s.strip_suffix("++") {
        (prefix, "+")
    } else if let Some((m, k)) = s.rsplit_once('+') {
        (m, k)
    } else {
        ("", s)
    };
    let mut mods = Vec::new();
    if !mod_part.is_empty() {
        for tok in mod_part.split('+') {
            let tok = tok.trim();
            if tok.is_empty() {
                return Err(invalid(format!("malformed key combination {spec:?}")));
            }
            let m = parse_modifier(tok)?;
            if !mods.contains(&m) {
                mods.push(m);
            }
        }
    }
    let key = key.trim();
    if key.is_empty() {
        return Err(invalid(format!("malformed key combination {spec:?}")));
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap();
    let sym = if chars.next().is_none() {
        if first.is_control() {
            return Err(invalid(format!("unsupported key {key:?}")));
        }
        // "A" alone types a capital A; with modifiers ("ctrl+A") the letter
        // names the key and Shift is only pressed when listed.
        let c = if mods.is_empty() {
            first
        } else {
            first.to_ascii_lowercase()
        };
        char_keysym(c).ok_or_else(|| invalid(format!("unsupported key {key:?}")))?
    } else {
        named_keysym(key).ok_or_else(|| invalid(format!("unsupported key name {key:?}")))?
    };
    Ok((mods, sym))
}

// ---------------------------------------------------------------------------
// Pixel conversion
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct PixelLayout {
    bpp: u8,
    stride: usize,
    lsb_first: bool,
    red: u32,
    green: u32,
    blue: u32,
}

fn mask_shift_bits(mask: u32) -> (u32, u32) {
    if mask == 0 {
        return (0, 0);
    }
    (
        mask.trailing_zeros(),
        (mask >> mask.trailing_zeros()).trailing_ones(),
    )
}

fn scale_channel(pixel: u32, mask: u32) -> u8 {
    let (shift, bits) = mask_shift_bits(mask);
    if bits == 0 {
        return 0;
    }
    let v = (pixel & mask) >> shift;
    if bits >= 8 {
        (v >> (bits - 8)) as u8
    } else {
        let max = (1u32 << bits) - 1;
        ((v * 255 + max / 2) / max) as u8
    }
}

fn to_rgb(data: &[u8], width: usize, height: usize, l: &PixelLayout) -> Result<Vec<u8>> {
    let bytes_pp = match l.bpp {
        16 => 2,
        24 => 3,
        32 => 4,
        other => {
            return Err(xerr(
                "capture_failed",
                format!("unsupported pixmap format: {other} bits per pixel"),
            ))
        }
    };
    if l.stride < width * bytes_pp
        || data.len() < l.stride * height.saturating_sub(1) + width * bytes_pp
    {
        return Err(xerr("capture_failed", "image data shorter than expected"));
    }
    let mut out = Vec::with_capacity(width * height * 3);
    let fast =
        l.bpp == 32 && l.lsb_first && l.red == 0xff0000 && l.green == 0xff00 && l.blue == 0xff;
    for y in 0..height {
        let row = &data[y * l.stride..y * l.stride + width * bytes_pp];
        if fast {
            for px in row.as_chunks::<4>().0 {
                out.extend_from_slice(&[px[2], px[1], px[0]]);
            }
            continue;
        }
        for px in row.chunks_exact(bytes_pp) {
            let v = match (bytes_pp, l.lsb_first) {
                (2, true) => u16::from_le_bytes([px[0], px[1]]) as u32,
                (2, false) => u16::from_be_bytes([px[0], px[1]]) as u32,
                (3, true) => u32::from_le_bytes([px[0], px[1], px[2], 0]),
                (3, false) => u32::from_be_bytes([0, px[0], px[1], px[2]]),
                (_, true) => u32::from_le_bytes([px[0], px[1], px[2], px[3]]),
                (_, false) => u32::from_be_bytes([px[0], px[1], px[2], px[3]]),
            };
            out.extend_from_slice(&[
                scale_channel(v, l.red),
                scale_channel(v, l.green),
                scale_channel(v, l.blue),
            ]);
        }
    }
    Ok(out)
}

fn encode_png(rgb: &[u8], width: u16, height: u16) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(rgb.len() / 4 + 1024);
    {
        let mut enc = png::Encoder::new(&mut out, width as u32, height as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut w = enc.write_header().context("png header")?;
        w.write_image_data(rgb).context("png encode")?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// MIT-SHM buffer
// ---------------------------------------------------------------------------

struct ShmBuf {
    seg: shm::Seg,
    ptr: *mut u8,
    len: usize,
}

impl Drop for ShmBuf {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

impl ShmBuf {
    fn create(conn: &RustConnection, len: usize) -> Result<Self> {
        let name = c"linux-cua-capture";
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("memfd_create");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(std::io::Error::last_os_error()).context("ftruncate");
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mmap");
        }
        let mut buf = ShmBuf {
            seg: 0,
            ptr: ptr as *mut u8,
            len,
        };
        let seg = conn.generate_id()?;
        // attach_fd takes ownership of the descriptor and closes it after sending.
        conn.shm_attach_fd(seg, fd, false)?
            .check()
            .context("shm attach")?;
        buf.seg = seg;
        Ok(buf)
    }

    fn bytes(&self, n: usize) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, n.min(self.len)) }
    }
}

// ---------------------------------------------------------------------------
// Desktop
// ---------------------------------------------------------------------------

/// Composite/Damage state of a window (or popup) whose pixels this session
/// reads. Keyed by XID but bound to the owning PID: a reused XID of another
/// process starts fresh.
struct Tracked {
    pid: u32,
    damage: damage::Damage,
    /// Set while the backing pixmap may not hold the client's own pixels yet
    /// (fresh redirect or resize): damage with at least this event sequence
    /// number is the client's repaint.
    repaint_after: Option<u64>,
    last_damage: Instant,
    size: (u16, u16),
}

/// Resolved pointer target inside a window hierarchy.
#[derive(Clone, Copy, Debug)]
struct Target {
    win: Window,
    /// Offset that converts client-local coordinates into `win` coordinates.
    dx: i32,
    dy: i32,
    /// Offset that converts client-local coordinates into root coordinates.
    rdx: i32,
    rdy: i32,
}

impl Target {
    fn local(&self, x: i32, y: i32) -> (i16, i16) {
        (clamp16(x + self.dx), clamp16(y + self.dy))
    }
    fn root(&self, x: i32, y: i32) -> (i16, i16) {
        (clamp16(x + self.rdx), clamp16(y + self.rdy))
    }
}

#[derive(Clone, Copy)]
struct KeyOrigin {
    client: Window,
    ex: i16,
    ey: i16,
    rx: i16,
    ry: i16,
}

fn clamp16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Copies `src` (`sw`x`sh` RGB) into `dst` (`dw`x`dh` RGB) at offset `(ox, oy)`,
/// clipped to `dst`.
fn blit(
    dst: &mut [u8],
    (dw, dh): (u16, u16),
    src: &[u8],
    (sw, sh): (u16, u16),
    (ox, oy): (i32, i32),
) {
    let (dw, dh, sw, sh) = (dw as i32, dh as i32, sw as i32, sh as i32);
    let x0 = ox.max(0);
    let x1 = (ox + sw).min(dw);
    if x1 <= x0 {
        return;
    }
    for y in oy.max(0)..(oy + sh).min(dh) {
        let s = (((y - oy) * sw + (x0 - ox)) * 3) as usize;
        let d = ((y * dw + x0) * 3) as usize;
        let n = ((x1 - x0) * 3) as usize;
        dst[d..d + n].copy_from_slice(&src[s..s + n]);
    }
}

pub struct Desktop {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    helper: Window,
    keymap: Keymap,
    shm_available: bool,
    shm: Option<ShmBuf>,
    shm_disabled: bool,
    tracked: HashMap<Window, Tracked>,
    /// Mapped override-redirect top-level windows -> (owner client base, PID).
    popups: HashMap<Window, (u32, u32)>,
    /// Top-levels of observed clients created but not yet mapped.
    creating: HashMap<Window, Instant>,
    abort: Option<String>,
    /// Relevant UI activity since `prepare_action`, and when it last happened.
    activity: bool,
    last_activity: Instant,
    /// Some of that activity was visible on X (damage, a window mapping or
    /// being created), not only accessibility events.
    x_activity: bool,
    cursors: HashMap<Window, (i32, i32)>,
    hover: HashMap<Window, Window>,
    settle_target: Option<Window>,
    live: Option<LiveScope>,
    display: String,
    /// CLIPBOARD owner (own connection and thread), started on first use.
    clipboard: Option<crate::clipboard::Owner>,
    /// Sequence number of the latest CLIPBOARD owner change XFixes reported.
    clipboard_seq: u64,
}

impl Desktop {
    pub fn connect(display: &str) -> Result<Self> {
        check_display_allowed(display)?;
        Self::connect_inner(display, None)
    }

    /// Explicit live-desktop opt-in (daemon command line only). May target
    /// display 0, but lists only windows of the allow-listed apps whose
    /// server-verified PID is known (and in `scope.pids` when given).
    pub fn connect_live(display: &str, scope: LiveScope) -> Result<Self> {
        display_number(display)?;
        if scope.apps.is_empty() || (scope.pids.is_empty() && scope.exes.is_empty()) {
            return Err(invalid(
                "live desktop mode requires --allow-app and at least one --pid or --allow-exe",
            ));
        }
        Self::connect_inner(display, Some(scope))
    }

    fn connect_inner(display: &str, live: Option<LiveScope>) -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(Some(display))
            .with_context(|| format!("connecting to X display {display}"))?;
        let root = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| anyhow::anyhow!("screen {screen_num} missing"))?
            .root;

        for (name, what) in [
            (composite::X11_EXTENSION_NAME, "Composite"),
            (damage::X11_EXTENSION_NAME, "DAMAGE"),
            (xfixes::X11_EXTENSION_NAME, "XFIXES"),
            (xinput::X11_EXTENSION_NAME, "XInputExtension"),
            (res::X11_EXTENSION_NAME, "X-Resource"),
        ] {
            if conn.extension_information(name)?.is_none() {
                return Err(xerr(
                    "unsupported_extension",
                    format!("X server lacks required {what} extension"),
                ));
            }
        }
        let cv = conn.composite_query_version(0, 4)?.reply()?;
        if (cv.major_version, cv.minor_version) < (0, 2) {
            return Err(xerr(
                "unsupported_extension",
                "Composite >= 0.2 required for NameWindowPixmap",
            ));
        }
        let rv = conn.res_query_version(1, 2)?.reply()?;
        if (rv.server_major, rv.server_minor) < (1, 2) {
            return Err(xerr(
                "unsupported_extension",
                "X-Resource >= 1.2 required for server-verified client PIDs",
            ));
        }
        conn.xfixes_query_version(5, 0)?.reply()?;
        conn.damage_query_version(1, 1)?.reply()?;
        let xi = conn.xinput_xi_query_version(2, 2)?.reply()?;
        if xi.major_version < 2 {
            return Err(xerr(
                "unsupported_extension",
                "XInput 2 required for raw input abort",
            ));
        }
        let shm_available = conn
            .extension_information(shm::X11_EXTENSION_NAME)?
            .is_some()
            && conn
                .shm_query_version()
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|v| (v.major_version, v.minor_version) >= (1, 2))
                .unwrap_or(false);

        let atoms = Atoms::new(&conn)?.reply()?;

        // Raw input from any physical (or other-client XTEST) device latches an abort.
        let mut mask = xinput::XIEventMask::RAW_KEY_PRESS
            | xinput::XIEventMask::RAW_BUTTON_PRESS
            | xinput::XIEventMask::RAW_MOTION;
        if (xi.major_version, xi.minor_version) >= (2, 2) {
            mask |= xinput::XIEventMask::RAW_TOUCH_BEGIN;
        }
        conn.xinput_xi_select_events(
            root,
            &[xinput::EventMask {
                deviceid: XI_ALL_MASTER_DEVICES,
                mask: vec![mask],
            }],
        )?
        .check()
        .context("selecting XI2 raw events")?;

        // New top-level / popup windows contribute to settle activity.
        conn.change_window_attributes(
            root,
            &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?
        .check()
        .context("selecting root substructure events")?;

        // Invisible helper for server timestamps. Never mapped.
        let helper = conn.generate_id()?;
        conn.create_window(
            0,
            helper,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(EventMask::PROPERTY_CHANGE),
        )?
        .check()
        .context("creating helper window")?;

        // Clipboard owner changes, so a read can tell that the owner it
        // authorized is still the one answering.
        if live.is_none() {
            conn.xfixes_select_selection_input(
                helper,
                atoms.CLIPBOARD,
                xfixes::SelectionEventMask::SET_SELECTION_OWNER
                    | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )?
            .check()
            .context("selecting clipboard owner events")?;
        }

        let keymap = Keymap::load(&conn)?;
        conn.flush()?;

        let mut desktop = Desktop {
            conn,
            root,
            atoms,
            helper,
            keymap,
            shm_available,
            shm: None,
            shm_disabled: false,
            tracked: HashMap::new(),
            popups: HashMap::new(),
            creating: HashMap::new(),
            abort: None,
            activity: false,
            last_activity: Instant::now(),
            x_activity: false,
            cursors: HashMap::new(),
            hover: HashMap::new(),
            settle_target: None,
            live,
            display: display.to_string(),
            clipboard: None,
            clipboard_seq: 0,
        };
        desktop.scan_popups()?;
        Ok(desktop)
    }

    /// The display this session is bound to (for processes it launches).
    pub fn display(&self) -> &str {
        &self.display
    }

    /// Whether this is the explicit live-desktop mode (the owner's display).
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    // ----- clipboard -------------------------------------------------------

    fn clipboard_owner(&mut self) -> Result<&crate::clipboard::Owner> {
        if self.live.is_some() {
            return Err(xerr(
                "unsupported",
                "the clipboard is not available on a live desktop (it belongs to the owner)",
            ));
        }
        if self.clipboard.as_ref().is_some_and(|o| !o.alive()) {
            self.clipboard = None;
        }
        if self.clipboard.is_none() {
            self.clipboard = Some(crate::clipboard::Owner::start(&self.display)?);
        }
        Ok(self.clipboard.as_ref().expect("started above"))
    }

    /// Makes `text` the CLIPBOARD contents, served by this daemon until an
    /// app takes the selection over or it is released.
    pub fn clipboard_write(&mut self, text: &str) -> Result<()> {
        self.clipboard_owner()?.set(text)
    }

    pub fn clipboard_release(&mut self) -> Result<()> {
        match &self.clipboard {
            Some(o) if o.alive() => o.release(),
            _ => Ok(()),
        }
    }

    /// The CLIPBOARD contents as text, `None` when nobody owns the
    /// selection or the owner offers no text. Text this daemon put there
    /// is returned as is. Otherwise the owner window must be a listed
    /// window, or the windows listed for its client (failing that, for its
    /// process) must all be of one app, and `authorize` must accept that
    /// window before the contents are requested. The read fails with
    /// `clipboard_changed` when the selection changes hands meanwhile.
    pub fn clipboard_read(
        &mut self,
        authorize: impl Fn(&WindowInfo) -> Result<()>,
    ) -> Result<Option<String>> {
        if self.live.is_some() {
            return Err(xerr(
                "unsupported",
                "the clipboard is not available on a live desktop (it belongs to the owner)",
            ));
        }
        if let Some(text) = self.clipboard.as_ref().and_then(|o| o.owned_text()) {
            return Ok(Some(text));
        }
        let cookie = self.conn.get_selection_owner(self.atoms.CLIPBOARD)?;
        // Owner changes that carry this sequence number or a later one
        // happened after `owner` was read (`handle` records them).
        let since = cookie.sequence_number();
        let owner = cookie.reply()?.owner;
        if owner == NONE {
            return Ok(None);
        }
        let changed = |d: &Self| -> Result<()> {
            if d.clipboard_seq >= since {
                return Err(xerr(
                    "clipboard_changed",
                    "the clipboard changed while it was read; nothing was read, read it again",
                ));
            }
            Ok(())
        };
        let (base, pid) = self
            .owners(&[owner])?
            .first()
            .copied()
            .flatten()
            .unwrap_or((self.client_base(owner), 0));
        let wins = self.describe(&self.candidates()?)?;
        let app = match wins.iter().find(|w| w.window_id == owner) {
            Some(w) => w,
            None => {
                let mut group: Vec<&WindowInfo> = wins
                    .iter()
                    .filter(|w| self.client_base(w.window_id) == base)
                    .collect();
                if group.is_empty() && pid != 0 {
                    group = wins.iter().filter(|w| w.pid == pid).collect();
                }
                let Some(&first) = group.first() else {
                    return Err(xerr(
                        "app_denied",
                        "the clipboard belongs to a client with no listed window, so its app cannot be checked; nothing was read",
                    ));
                };
                if group
                    .iter()
                    .any(|w| w.app_id != first.app_id || w.pid != first.pid)
                {
                    return Err(xerr(
                        "app_denied",
                        "the clipboard belongs to a client whose windows belong to several apps, so its app cannot be told; nothing was read",
                    ));
                }
                first
            }
        };
        authorize(app)?;
        let time = self.server_time()?;
        changed(self)?;
        self.conn.convert_selection(
            self.helper,
            self.atoms.CLIPBOARD,
            self.atoms.UTF8_STRING,
            self.atoms._LINUX_CUA_SELECTION,
            time,
        )?;
        self.conn.flush()?;
        let start = Instant::now();
        let property = loop {
            if let Some((ev, seq)) = self.conn.poll_for_event_with_sequence()? {
                match &ev {
                    Event::SelectionNotify(e)
                        if e.requestor == self.helper
                            && e.selection == self.atoms.CLIPBOARD
                            && e.time == time =>
                    {
                        break e.property;
                    }
                    _ => self.handle(ev, seq)?,
                }
                changed(self)?;
                continue;
            }
            let left = CLIPBOARD_WAIT.saturating_sub(start.elapsed());
            if left.is_zero() {
                return Err(xerr(
                    "timeout",
                    "the clipboard owner did not deliver its contents within 1.5 s",
                ));
            }
            self.wait_readable(left.min(Duration::from_millis(50)))?;
        };
        changed(self)?;
        if property == NONE {
            return Ok(None);
        }
        let reply = self
            .conn
            .get_property(
                true,
                self.helper,
                property,
                AtomEnum::ANY,
                0,
                CLIPBOARD_MAX / 4,
            )?
            .reply()?;
        self.conn.flush()?;
        // Events that arrived before the property's reply: an owner change
        // among them may have answered in place of the authorized owner.
        while let Some((ev, seq)) = self.conn.poll_for_event_with_sequence()? {
            self.handle(ev, seq)?;
        }
        changed(self)?;
        if reply.type_ == self.atoms.INCR {
            return Err(xerr(
                "unsupported",
                "the clipboard contents are too large (incremental transfer is not supported)",
            ));
        }
        if reply.format != 8 {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&reply.value).into_owned()))
    }

    // ----- client identity -------------------------------------------------

    /// The X server's ownership rule: the client that created a resource is
    /// encoded in the XID bits outside the resource-id mask (this is the
    /// value X-Resource reports as ClientXID).
    fn client_base(&self, xid: u32) -> u32 {
        xid & !self.conn.setup().resource_id_mask
    }

    /// X-Resource identity of the clients that own `wins`: `(client base,
    /// PID)`, with PID 0 when the server cannot report it. `None` when the
    /// owning client is gone.
    fn owners(&self, wins: &[Window]) -> Result<Vec<Option<(u32, u32)>>> {
        if wins.is_empty() {
            return Ok(Vec::new());
        }
        let specs: Vec<res::ClientIdSpec> = wins
            .iter()
            .map(|&w| res::ClientIdSpec {
                client: w,
                mask: res::ClientIdMask::CLIENT_XID | res::ClientIdMask::LOCAL_CLIENT_PID,
            })
            .collect();
        let reply = self.conn.res_query_client_ids(&specs)?.reply()?;
        let mut present = HashSet::new();
        let mut pids = HashMap::new();
        for id in reply.ids {
            let mask = u32::from(id.spec.mask);
            if mask == u32::from(res::ClientIdMask::CLIENT_XID) {
                present.insert(id.spec.client);
            } else if mask == u32::from(res::ClientIdMask::LOCAL_CLIENT_PID) {
                if let Some(&pid) = id.value.first() {
                    pids.insert(id.spec.client, pid);
                }
            }
        }
        Ok(wins
            .iter()
            .map(|&w| {
                let base = self.client_base(w);
                present
                    .contains(&base)
                    .then(|| (base, pids.get(&base).copied().unwrap_or(0)))
            })
            .collect())
    }

    /// Records override-redirect windows that were mapped before we connected.
    fn scan_popups(&mut self) -> Result<()> {
        let tree = self.conn.query_tree(self.root)?.reply()?;
        let cookies: Vec<_> = tree
            .children
            .iter()
            .map(|&w| (w, self.conn.get_window_attributes(w)))
            .collect();
        let mut found = Vec::new();
        for (w, c) in cookies {
            let Ok(a) = c?.reply() else { continue };
            if w != self.helper && a.override_redirect && a.map_state == MapState::VIEWABLE {
                found.push(w);
            }
        }
        for (w, owner) in found.iter().zip(self.owners(&found)?) {
            if let Some(o) = owner {
                self.popups.insert(*w, o);
            }
        }
        Ok(())
    }

    // ----- window enumeration ---------------------------------------------

    fn has_property(&self, win: Window, prop: u32) -> bool {
        self.conn
            .get_property(false, win, prop, AtomEnum::ANY, 0, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.type_ != NONE)
            .unwrap_or(false)
    }

    fn find_wm_state_client(&self, win: Window, depth: u32) -> Option<Window> {
        if self.has_property(win, self.atoms.WM_STATE) {
            return Some(win);
        }
        if depth == 0 {
            return None;
        }
        let tree = self.conn.query_tree(win).ok()?.reply().ok()?;
        tree.children
            .iter()
            .rev()
            .find_map(|&c| self.find_wm_state_client(c, depth - 1))
    }

    /// Candidate client windows: `_NET_CLIENT_LIST` when an EWMH window
    /// manager publishes it, otherwise a walk of root's mapped children.
    fn candidates(&self) -> Result<Vec<Window>> {
        let r = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms._NET_CLIENT_LIST,
                AtomEnum::WINDOW,
                0,
                8192,
            )?
            .reply()?;
        if r.type_ == u32::from(AtomEnum::WINDOW) && r.format == 32 {
            if let Some(v) = r.value32() {
                return Ok(v.filter(|&w| w != self.helper && w != self.root).collect());
            }
        }
        let tree = self.conn.query_tree(self.root)?.reply()?;
        let cookies: Vec<_> = tree
            .children
            .iter()
            .map(|&w| (w, self.conn.get_window_attributes(w)))
            .collect();
        let mut out = Vec::new();
        for (w, c) in cookies {
            let Ok(a) = c?.reply() else { continue };
            if w == self.helper
                || a.override_redirect
                || a.map_state != MapState::VIEWABLE
                || a.class != WindowClass::INPUT_OUTPUT
            {
                continue;
            }
            if let Some(client) = self.find_wm_state_client(w, 3) {
                out.push(client);
            } else if self.has_property(w, AtomEnum::WM_CLASS.into()) {
                // No window manager: top-level application windows directly.
                out.push(w);
            }
        }
        Ok(out)
    }

    fn describe(&self, wins: &[Window]) -> Result<Vec<WindowInfo>> {
        let a = &self.atoms;
        let mut pending = Vec::with_capacity(wins.len());
        for &w in wins {
            pending.push((
                w,
                self.conn.get_window_attributes(w)?,
                self.conn.get_geometry(w)?,
                self.conn
                    .get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)?,
                self.conn
                    .get_property(false, w, a._NET_WM_NAME, a.UTF8_STRING, 0, 4096)?,
                self.conn
                    .get_property(false, w, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 4096)?,
                self.conn.get_property(
                    false,
                    w,
                    AtomEnum::WM_TRANSIENT_FOR,
                    AtomEnum::WINDOW,
                    0,
                    1,
                )?,
                self.conn.translate_coordinates(w, self.root, 0, 0)?,
            ));
        }
        let owners = self.owners(wins)?;
        let mut out = Vec::new();
        for ((w, attrs, geom, class, net_name, name, transient, origin), owner) in
            pending.into_iter().zip(owners)
        {
            let origin = origin
                .reply()
                .map(|o| (o.dst_x as i32, o.dst_y as i32))
                .unwrap_or_default();
            let transient_for = transient
                .reply()
                .ok()
                .and_then(|r| r.value32().and_then(|mut v| v.next()))
                .filter(|&t| t != NONE && t != w && t != self.root);
            let (Ok(attrs), Ok(geom)) = (attrs.reply(), geom.reply()) else {
                let _ = (class.reply(), net_name.reply(), name.reply());
                continue;
            };
            let class = class.reply().ok();
            let net_name = net_name.reply().ok();
            let name = name.reply().ok();
            if attrs.map_state != MapState::VIEWABLE
                || attrs.class != WindowClass::INPUT_OUTPUT
                || geom.width == 0
                || geom.height == 0
            {
                continue;
            }
            let app_id = class
                .map(|c| {
                    let parts: Vec<String> = c
                        .value
                        .split(|&b| b == 0)
                        .filter(|s| !s.is_empty())
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                        .collect();
                    parts.get(1).or(parts.first()).cloned().unwrap_or_default()
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown".to_string());
            let title = match net_name
                .filter(|r| r.type_ == self.atoms.UTF8_STRING && !r.value.is_empty())
            {
                Some(r) => String::from_utf8_lossy(&r.value).into_owned(),
                None => name
                    .map(|r| {
                        if r.type_ == u32::from(AtomEnum::STRING) {
                            r.value.iter().map(|&b| b as char).collect()
                        } else {
                            String::from_utf8_lossy(&r.value).into_owned()
                        }
                    })
                    .unwrap_or_default(),
            };
            let info = WindowInfo {
                window_id: w,
                title,
                app_id,
                pid: owner.map(|o| o.1).unwrap_or(0),
                width: geom.width,
                height: geom.height,
                transient_for,
                origin,
            };
            if self.live.as_ref().is_some_and(|l| !l.admits(&info)) {
                continue;
            }
            out.push(info);
        }
        Ok(out)
    }

    pub fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        let wins = self.candidates()?;
        let infos = self.describe(&wins)?;
        // Stop tracking windows that are gone or no longer listed.
        let live: HashMap<Window, u32> = infos.iter().map(|i| (i.window_id, i.pid)).collect();
        let stale: Vec<Window> = self
            .tracked
            .iter()
            .filter(|(w, t)| !self.popups.contains_key(w) && live.get(w) != Some(&t.pid))
            .map(|(w, _)| *w)
            .collect();
        for w in stale {
            self.untrack(w);
        }
        let _ = self.conn.flush();
        Ok(infos)
    }

    /// Validated lookup: the id must be a currently listed client window.
    pub fn window(&self, id: u32) -> Result<WindowInfo> {
        if !self.candidates()?.contains(&id) {
            return Err(xerr(
                "window_not_found",
                format!("window {id} is not a listed application window"),
            ));
        }
        self.describe(&[id])?
            .into_iter()
            .next()
            .ok_or_else(|| xerr("window_not_found", format!("window {id} is not viewable")))
    }

    /// Listed windows of one app: the same server-verified process and app ID
    /// (main windows, dialogs, other top-levels), in stacking order bottom to
    /// top when the window manager publishes it.
    pub fn app_windows(&self, pid: u32, app_id: &str) -> Result<Vec<WindowInfo>> {
        if pid == 0 {
            return Ok(Vec::new());
        }
        let wins = self.candidates()?;
        let mut out: Vec<WindowInfo> = self
            .describe(&wins)?
            .into_iter()
            .filter(|w| w.pid == pid && w.app_id == app_id)
            .collect();
        let order = self.stacking().unwrap_or(wins);
        out.sort_by_key(|w| order.iter().position(|&o| o == w.window_id));
        Ok(out)
    }

    fn stacking(&self) -> Option<Vec<Window>> {
        let r = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms._NET_CLIENT_LIST_STACKING,
                AtomEnum::WINDOW,
                0,
                8192,
            )
            .ok()?
            .reply()
            .ok()?;
        let v: Vec<Window> = r.value32()?.collect();
        (!v.is_empty()).then_some(v)
    }

    /// The window of an app that keys, text and observations go to: the one
    /// holding the X input focus, else its topmost dialog (WM_TRANSIENT_FOR),
    /// else `preferred`, else its topmost window. `wins` comes from
    /// `app_windows` (stacking order).
    pub fn active_window(&self, wins: &[WindowInfo], preferred: u32) -> Option<WindowInfo> {
        if let Some(f) = self.focused_of(wins) {
            return wins.iter().find(|w| w.window_id == f).cloned();
        }
        wins.iter()
            .rev()
            .find(|w| w.transient_for.is_some())
            .or_else(|| wins.iter().find(|w| w.window_id == preferred))
            .or_else(|| wins.last())
            .cloned()
    }

    /// The window among `wins` that holds (or contains) the input focus.
    pub fn focused_of(&self, wins: &[WindowInfo]) -> Option<Window> {
        let ids: HashSet<Window> = wins.iter().map(|w| w.window_id).collect();
        let f = self.conn.get_input_focus().ok()?.reply().ok()?;
        let mut cur = f.focus;
        for _ in 0..16 {
            if cur == NONE || cur == self.root || cur == 1 {
                return None;
            }
            if ids.contains(&cur) {
                return Some(cur);
            }
            cur = self.conn.query_tree(cur).ok()?.reply().ok()?.parent;
        }
        None
    }

    // ----- composite tracking ----------------------------------------------

    /// Eagerly redirects and damage-tracks an authorized window, so its
    /// backing pixmap holds the client's own pixels by the time it is read.
    pub fn track(&mut self, w: &WindowInfo) -> Result<()> {
        self.track_window(w.window_id, w.pid)?;
        self.conn.flush()?;
        Ok(())
    }

    fn track_window(&mut self, win: Window, pid: u32) -> Result<()> {
        match self.tracked.get(&win) {
            Some(t) if t.pid == pid => return Ok(()),
            Some(_) => self.untrack(win),
            None => {}
        }
        // Damage first, so the repaint triggered below is observed.
        let damage = self.conn.generate_id()?;
        self.conn
            .damage_create(damage, win, damage::ReportLevel::NON_EMPTY)?;
        // Resizes invalidate the backing pixmap; destruction ends tracking.
        self.conn.change_window_attributes(
            win,
            &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?;
        // BadAccess means a compositor holds a manual redirection; the window
        // then already has a backing pixmap. Errors arrive asynchronously.
        self.conn
            .composite_redirect_window(win, composite::Redirect::AUTOMATIC)?;
        // Empty the damage region before the repaint is requested: with
        // NON_EMPTY reporting, pending damage would swallow the repaint's notify.
        self.conn.damage_subtract(damage, NONE, NONE)?;
        // A fresh backing pixmap is initialised from the parent, including
        // pixels of overlapping windows. Exposures make the client repaint
        // all of it (the redirected window counts as fully visible). Damage
        // reported at or after this request's sequence number is that repaint.
        let fence = self
            .conn
            .clear_area(true, win, 0, 0, 0, 0)?
            .sequence_number();
        let geom = match self.conn.get_geometry(win)?.reply() {
            Ok(g) => g,
            Err(e) => {
                // Vanished (e.g. a tooltip unmapped and destroyed before we
                // got here). The server frees the damage with the drawable;
                // destroying it ourselves is harmless if it already went.
                let _ = self.conn.damage_destroy(damage);
                return Err(xerr("window_gone", format!("window {win:#x} is gone: {e}")));
            }
        };
        self.tracked.insert(
            win,
            Tracked {
                pid,
                damage,
                repaint_after: Some(fence),
                last_damage: Instant::now(),
                size: (geom.width, geom.height),
            },
        );
        Ok(())
    }

    fn untrack(&mut self, win: Window) {
        if let Some(t) = self.tracked.remove(&win) {
            let _ = self.conn.damage_destroy(t.damage);
            let _ = self
                .conn
                .composite_unredirect_window(win, composite::Redirect::AUTOMATIC);
            let _ = self.conn.change_window_attributes(
                win,
                &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
            );
        }
    }

    /// Waits (bounded) until pending client repaints of `wins` (after a fresh
    /// redirect or a resize) arrived and went quiet. Windows without a
    /// pending repaint cost nothing, even if they animate.
    fn await_repaint(&mut self, wins: &[Window]) -> Result<()> {
        self.pump()?;
        let pending: Vec<Window> = wins
            .iter()
            .copied()
            .filter(|w| {
                self.tracked
                    .get(w)
                    .is_some_and(|t| t.repaint_after.is_some())
            })
            .collect();
        let start = Instant::now();
        while !pending.is_empty() {
            let now = Instant::now();
            let waiting = pending
                .iter()
                .filter_map(|w| self.tracked.get(w))
                .map(|t| match t.repaint_after {
                    Some(_) => REPAINT_WAIT,
                    None => REPAINT_QUIET.saturating_sub(now - t.last_damage),
                })
                .max()
                .unwrap_or_default();
            let left = REPAINT_WAIT.saturating_sub(now - start);
            if waiting.is_zero() || left.is_zero() {
                return Ok(());
            }
            self.wait_readable(waiting.min(left))?;
            self.pump()?;
        }
        Ok(())
    }

    /// Mapped popups of the window's client, bottom to top.
    fn client_popups(&self, win: Window) -> Result<Vec<(Window, u32)>> {
        let base = self.client_base(win);
        let mine: HashMap<Window, u32> = self
            .popups
            .iter()
            .filter(|(_, (b, _))| *b == base)
            .map(|(w, (_, pid))| (*w, *pid))
            .collect();
        if mine.is_empty() {
            return Ok(Vec::new());
        }
        let tree = self.conn.query_tree(self.root)?.reply()?;
        Ok(tree
            .children
            .into_iter()
            .filter_map(|c| mine.get(&c).map(|&pid| (c, pid)))
            .collect())
    }

    // ----- capture ---------------------------------------------------------

    fn visual_masks(&self, visual: u32) -> Option<(u32, u32, u32)> {
        self.conn
            .setup()
            .roots
            .iter()
            .flat_map(|s| s.allowed_depths.iter())
            .flat_map(|d| d.visuals.iter())
            .find(|v| v.visual_id == visual)
            .map(|v| (v.red_mask, v.green_mask, v.blue_mask))
    }

    fn layout_for(&self, depth: u8, width: u16, visual: u32) -> Result<PixelLayout> {
        let setup = self.conn.setup();
        let fmt = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .ok_or_else(|| {
                xerr(
                    "capture_failed",
                    format!("no pixmap format for depth {depth}"),
                )
            })?;
        let (red, green, blue) = self
            .visual_masks(visual)
            .ok_or_else(|| xerr("capture_failed", format!("visual {visual:#x} not found")))?;
        if red == 0 || green == 0 || blue == 0 {
            return Err(xerr(
                "capture_failed",
                "non-TrueColor visuals are not supported",
            ));
        }
        let pad = fmt.scanline_pad.max(8) as usize;
        let bits = width as usize * fmt.bits_per_pixel as usize;
        let stride = bits.div_ceil(pad) * pad / 8;
        Ok(PixelLayout {
            bpp: fmt.bits_per_pixel,
            stride,
            lsb_first: setup.image_byte_order == ImageOrder::LSB_FIRST,
            red,
            green,
            blue,
        })
    }

    /// Reads a `w`x`h` rectangle at `(x, y)` of a drawable as RGB, preferring
    /// MIT-SHM and falling back to core GetImage on any SHM failure.
    fn read_drawable(
        &mut self,
        drawable: u32,
        (x, y, w, h): (i16, i16, u16, u16),
        layout: &PixelLayout,
    ) -> Result<Vec<u8>> {
        let needed = layout.stride * h as usize;
        if self.shm_available && !self.shm_disabled {
            if self.shm.as_ref().map(|b| b.len < needed).unwrap_or(true) {
                if let Some(old) = self.shm.take() {
                    let _ = self.conn.shm_detach(old.seg);
                }
                let len = needed.div_ceil(1 << 20) * (1 << 20);
                match ShmBuf::create(&self.conn, len) {
                    Ok(b) => self.shm = Some(b),
                    Err(_) => self.shm_disabled = true,
                }
            }
            if let Some(buf) = self.shm.as_ref() {
                let r = self
                    .conn
                    .shm_get_image(
                        drawable,
                        x,
                        y,
                        w,
                        h,
                        !0,
                        ImageFormat::Z_PIXMAP.into(),
                        buf.seg,
                        0,
                    )
                    .map_err(anyhow::Error::from)
                    .and_then(|c| c.reply().map_err(anyhow::Error::from));
                if let Ok(reply) = r {
                    if reply.size as usize >= needed {
                        return to_rgb(
                            buf.bytes(reply.size as usize),
                            w as usize,
                            h as usize,
                            layout,
                        );
                    }
                }
                // Fall through to the core path for this capture only.
            }
        }
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, drawable, x, y, w, h, !0)?
            .reply()
            .map_err(|e| xerr("capture_failed", format!("GetImage failed: {e}")))?;
        to_rgb(&reply.data, w as usize, h as usize, layout)
    }

    /// Reads a redirected window's own pixels from its Composite backing
    /// pixmap (never from the screen, so occluding windows cannot leak in).
    fn read_window(&mut self, win: Window) -> Result<(u16, u16, Vec<u8>)> {
        let attrs = self.conn.get_window_attributes(win)?.reply()?;
        let geom = self.conn.get_geometry(win)?.reply().map_err(|e| {
            xerr(
                "capture_failed",
                format!("window geometry unavailable: {e}"),
            )
        })?;
        let pixmap = self.conn.generate_id()?;
        if self
            .conn
            .composite_name_window_pixmap(win, pixmap)?
            .check()
            .is_err()
        {
            return Err(xerr(
                "capture_failed",
                "Composite backing pixmap unavailable; refusing an occlusion-sensitive capture",
            ));
        }
        // The backing pixmap includes the border.
        let bw = geom.border_width as i16;
        let result = self
            .layout_for(geom.depth, geom.width, attrs.visual)
            .and_then(|layout| {
                self.read_drawable(pixmap, (bw, bw, geom.width, geom.height), &layout)
            });
        let _ = self.conn.free_pixmap(pixmap);
        let _ = self.conn.flush();
        Ok((geom.width, geom.height, result?))
    }

    /// The window's pixels with its client's mapped popups (menus, dropdowns,
    /// tooltips) composited on top, clipped to the window.
    fn capture(&mut self, w: &WindowInfo) -> Result<(u16, u16, Vec<u8>)> {
        let win = w.window_id;
        self.track_window(win, w.pid)?;
        let mut popups = self.client_popups(win)?;
        // Popups that vanished since they were mapped are skipped.
        popups.retain(|&(p, pid)| self.track_window(p, pid).is_ok());
        let mut all: Vec<Window> = popups.iter().map(|p| p.0).collect();
        all.push(win);
        self.await_repaint(&all)?;
        let (width, height, mut rgb) = self.read_window(win)?;
        for (p, _) in popups {
            // A popup can vanish between listing and reading.
            let Ok((pw, ph, prgb)) = self.read_window(p) else {
                continue;
            };
            let Ok(o) = self
                .conn
                .translate_coordinates(p, win, 0, 0)
                .map_err(anyhow::Error::from)
                .and_then(|c| c.reply().map_err(anyhow::Error::from))
            else {
                continue;
            };
            blit(
                &mut rgb,
                (width, height),
                &prgb,
                (pw, ph),
                (o.dst_x as i32, o.dst_y as i32),
            );
        }
        Ok((width, height, rgb))
    }

    pub fn screenshot(&mut self, w: &WindowInfo) -> Result<Value> {
        let (width, height, rgb) = self.capture(w)?;
        let png = encode_png(&rgb, width, height)?;
        Ok(json!({
            "data": base64::engine::general_purpose::STANDARD.encode(png),
            "mimeType": "image/png",
            "width": width,
            "height": height,
        }))
    }

    // ----- event pump ------------------------------------------------------

    fn latch(&mut self, what: String) {
        if self.abort.is_none() {
            self.abort = Some(what);
        }
    }

    /// Whether `d` belongs to the current settle target: the window itself
    /// or a popup (or any window) of the same X client.
    fn relevant(&self, d: Window) -> bool {
        match self.settle_target {
            Some(t) => d == t || self.client_base(d) == self.client_base(t),
            None => false,
        }
    }

    /// Activity visible on X (the event handlers).
    fn note_activity(&mut self) {
        self.activity = true;
        self.x_activity = true;
        self.last_activity = Instant::now();
    }

    /// Activity reported by the accessibility probe.
    fn note_external_activity(&mut self) {
        self.activity = true;
        self.last_activity = Instant::now();
    }

    fn handle(&mut self, ev: Event, seq: u64) -> Result<()> {
        match ev {
            Event::XinputRawKeyPress(e) => {
                self.latch(format!("physical key press from device {}", e.sourceid))
            }
            Event::XinputRawButtonPress(e) => {
                self.latch(format!("physical button press from device {}", e.sourceid))
            }
            Event::XinputRawMotion(e) => self.latch(format!(
                "physical pointer motion from device {}",
                e.sourceid
            )),
            Event::XinputRawTouchBegin(e) => {
                self.latch(format!("physical touch from device {}", e.sourceid))
            }
            Event::DamageNotify(e) => {
                let _ = self.conn.damage_subtract(e.damage, NONE, NONE);
                if let Some(t) = self.tracked.get_mut(&e.drawable) {
                    t.last_damage = Instant::now();
                    if t.repaint_after.is_some_and(|fence| seq >= fence) {
                        t.repaint_after = None;
                    }
                }
                if self.relevant(e.drawable) {
                    self.note_activity();
                }
            }
            Event::XfixesSelectionNotify(e) if e.selection == self.atoms.CLIPBOARD => {
                self.clipboard_seq = self.clipboard_seq.max(seq);
            }
            Event::MapNotify(e) if e.event == self.root => {
                if e.override_redirect && e.window != self.helper {
                    // A popup can be unmapped and destroyed before we get
                    // here; it is then simply gone. Popup tracking never
                    // fails the pump (input may already have been sent).
                    if let Some(Some(owner)) = self
                        .owners(&[e.window])
                        .ok()
                        .and_then(|o| o.first().copied())
                    {
                        self.popups.insert(e.window, owner);
                        // Popups of clients we read are redirected right away.
                        let base = owner.0;
                        if self.tracked.keys().any(|&w| self.client_base(w) == base)
                            && self.track_window(e.window, owner.1).is_err()
                        {
                            self.popups.remove(&e.window);
                        }
                    }
                }
                if self.relevant(e.window) {
                    self.note_activity();
                }
            }
            Event::UnmapNotify(e) if e.event == self.root => {
                if self.popups.remove(&e.window).is_some() {
                    self.untrack(e.window);
                }
                if self.relevant(e.window) {
                    self.note_activity();
                }
            }
            Event::CreateNotify(e)
                if e.parent == self.root
                    && !e.override_redirect
                    && self
                        .settle_target
                        .is_some_and(|t| self.client_base(t) == self.client_base(e.window)) =>
            {
                // The app being acted on is building a new top-level (a
                // dialog): settling waits for it to map. Its own MapNotify is
                // selected because a reparenting window manager reports the
                // map to the frame, not to the root; a map that happened
                // before the selection is caught by the attribute check.
                let _ = self.conn.change_window_attributes(
                    e.window,
                    &xproto::ChangeWindowAttributesAux::new()
                        .event_mask(EventMask::STRUCTURE_NOTIFY),
                );
                let viewable = self
                    .conn
                    .get_window_attributes(e.window)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|a| a.map_state != xproto::MapState::UNMAPPED);
                if viewable == Some(false) {
                    self.creating.insert(e.window, Instant::now());
                }
                if crate::trace_on() {
                    eprintln!(
                        "trace x11 CreateNotify {:#x} viewable={viewable:?} t={:?}",
                        e.window,
                        crate::since_start()
                    );
                }
                self.note_activity();
            }
            Event::MapNotify(e) if self.creating.remove(&e.window).is_some() => {
                if crate::trace_on() {
                    eprintln!(
                        "trace x11 MapNotify {:#x} t={:?}",
                        e.window,
                        crate::since_start()
                    );
                }
                self.note_activity();
            }
            Event::DestroyNotify(e) if e.event == self.root || e.event == e.window => {
                self.creating.remove(&e.window);
                // The server frees damage objects with their drawable.
                self.popups.remove(&e.window);
                self.tracked.remove(&e.window);
            }
            Event::ConfigureNotify(e) if e.event == e.window => {
                if let Some(t) = self.tracked.get_mut(&e.window) {
                    if t.size != (e.width, e.height) {
                        t.size = (e.width, e.height);
                        let _ = self.conn.damage_subtract(t.damage, NONE, NONE);
                        // The resized pixmap is valid once the client repaints.
                        t.repaint_after = Some(seq);
                    }
                }
            }
            Event::MappingNotify(e) if e.request != xproto::Mapping::POINTER => {
                self.keymap = Keymap::load(&self.conn)?;
            }
            // Asynchronous errors from unchecked requests on vanished windows.
            _ => {}
        }
        Ok(())
    }

    fn pump(&mut self) -> Result<()> {
        while let Some((ev, seq)) = self.conn.poll_for_event_with_sequence()? {
            self.handle(ev, seq)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn wait_readable(&self, timeout: Duration) -> Result<()> {
        let mut pfd = libc::pollfd {
            fd: self.conn.stream().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err).context("poll on X connection");
            }
        }
        Ok(())
    }

    /// A real server timestamp (via a zero-length property append).
    fn server_time(&mut self) -> Result<u32> {
        self.conn.change_property8(
            PropMode::APPEND,
            self.helper,
            self.atoms._LINUX_CUA_TIME,
            AtomEnum::STRING,
            &[],
        )?;
        self.conn.flush()?;
        loop {
            let (ev, seq) = self.conn.wait_for_event_with_sequence()?;
            if let Event::PropertyNotify(e) = &ev {
                if e.window == self.helper && e.atom == self.atoms._LINUX_CUA_TIME {
                    return Ok(e.time);
                }
            }
            self.handle(ev, seq)?;
        }
    }

    pub fn check_user_input(&mut self) -> Result<()> {
        self.pump()?;
        match &self.abort {
            Some(what) => Err(xerr(
                "user_input_abort",
                format!("user input detected: {what}"),
            )),
            None => Ok(()),
        }
    }

    pub fn clear_input_abort(&mut self) -> Result<()> {
        self.pump()?;
        self.abort = None;
        Ok(())
    }

    /// Forgets per-turn input state: abort latch, virtual cursors, hover
    /// windows and the settle target.
    pub fn end_turn(&mut self) -> Result<()> {
        self.clear_input_abort()?;
        self.cursors.clear();
        self.hover.clear();
        self.settle_target = None;
        Ok(())
    }

    /// Arms activity tracking for the action that follows (semantic or X11).
    pub fn prepare_action(&mut self, w: &WindowInfo) -> Result<()> {
        self.check_user_input()?;
        self.track_window(w.window_id, w.pid)?;
        self.settle_target = Some(w.window_id);
        self.activity = false;
        self.x_activity = false;
        // Windows another action left half-created do not hold this one.
        self.creating.clear();
        self.conn.flush()?;
        if crate::trace_on() {
            eprintln!("trace x11 action_start t={:?}", crate::since_start());
        }
        Ok(())
    }

    /// Two-phase settle after an action. Phase 1 waits up to `react` for the
    /// first sign that the app reacted: damage on its window or popups, a
    /// window of the same client (un)mapping, or `external()` reporting an
    /// accessibility event of the app. Activity seen while the action ran
    /// counts. Phase 2 waits until that activity has been quiet for `quiet`.
    /// Everything is bounded by `cap`, except that a top-level the target app
    /// is creating extends the wait until it maps, up to `MAP_WAIT`; a
    /// latched user-input abort ends early.
    pub fn settle(
        &mut self,
        react: Duration,
        quiet: Duration,
        cap: Duration,
        external: &mut dyn FnMut() -> bool,
    ) -> Result<Settled> {
        // Round trip: everything we sent has been processed by the server.
        self.conn.get_input_focus()?.reply()?;
        let start = Instant::now();
        loop {
            self.pump()?;
            if external() {
                self.note_external_activity();
            }
            if self.abort.is_some() {
                return Ok(Settled {
                    reacted: self.activity,
                    quiet: false,
                });
            }
            let now = Instant::now();
            let elapsed = now - start;
            // A new top-level of an observed client is still being built.
            self.creating.retain(|_, t| now - *t < MAP_WAIT);
            if !self.creating.is_empty() && elapsed < MAP_WAIT {
                self.wait_readable(POLL_SLICE)?;
                continue;
            }
            let left = if self.activity {
                // Accessibility events without anything shown yet: the app
                // is still at work (building a dialog), with pauses.
                let quiet = if self.x_activity {
                    quiet
                } else {
                    quiet.max(QUIET_INVISIBLE)
                };
                let quiet_for = now - self.last_activity.max(start);
                if quiet_for >= quiet {
                    return Ok(Settled {
                        reacted: true,
                        quiet: true,
                    });
                }
                quiet - quiet_for
            } else {
                if elapsed >= react {
                    return Ok(Settled {
                        reacted: false,
                        quiet: true,
                    });
                }
                react - elapsed
            };
            if elapsed >= cap {
                return Ok(Settled {
                    reacted: self.activity,
                    quiet: false,
                });
            }
            self.wait_readable(left.min(cap - elapsed).min(POLL_SLICE))?;
        }
    }

    /// Waits up to `max` for `window` to stop being viewable (a dialog
    /// closing after its confirm key or button). True if it closed.
    /// Waits up to `max` for `window` to stop being viewable (a dialog
    /// closing after its confirm key or button). True if it closed.
    ///
    /// The wait ends early when the dialog is evidently staying open: after
    /// activity during the wait (a folder listing loading, Find matching),
    /// `idle` without further activity; with no activity since the settle,
    /// `still` (longer: the GTK file chooser closes about 400 ms after
    /// Return with no activity at all before it unmaps).
    pub fn wait_closed(
        &mut self,
        window: Window,
        max: Duration,
        idle: Duration,
        still: Duration,
        external: &mut dyn FnMut() -> bool,
    ) -> Result<bool> {
        let mut active = false;
        let start = Instant::now();
        let mut trace = crate::Trace::new(format!("wait_closed {window:#x}"));
        loop {
            self.pump()?;
            let before = self.last_activity;
            if external() {
                self.note_external_activity();
            }
            if self.last_activity != before {
                if !active {
                    trace.mark("first_activity");
                }
                active = true;
            }
            if self.abort.is_some() {
                return Ok(false);
            }
            let open = match self.conn.get_window_attributes(window)?.reply() {
                Ok(a) => a.map_state == xproto::MapState::VIEWABLE,
                Err(_) => false,
            };
            if !open {
                trace.mark("closed");
                trace.done();
                return Ok(true);
            }
            let now = Instant::now();
            if close_wait_over(
                active,
                now - self.last_activity,
                now - start,
                idle,
                still,
                max,
            ) {
                trace.mark("gave_up");
                trace.note("active", active);
                trace.note("since_activity_ms", (now - self.last_activity).as_millis());
                trace.done();
                return Ok(false);
            }
            self.wait_readable(Duration::from_millis(20).min(max.saturating_sub(now - start)))?;
        }
    }

    // ----- input -----------------------------------------------------------

    /// Current virtual cursor position (window-local) for a window, if any
    /// pointer action has targeted it.
    pub fn virtual_cursor(&self, id: u32) -> Option<(i32, i32)> {
        self.cursors.get(&id).copied()
    }

    /// Finds the deepest mapped subwindow at a window-local point, looking at
    /// the client's popups (topmost first) before the window itself. Points
    /// outside the window (the captured area), and subwindows owned by
    /// another X client (XEmbed or reparented foreign windows), are refused.
    /// Only `click_element_point` lifts the window clip, for an accessible
    /// element of a popup that extends beyond the window.
    fn hit_test(&self, info: &WindowInfo, x: i32, y: i32, area: HitArea) -> Result<Target> {
        let client = info.window_id;
        let (x16, y16) = (clamp16(x), clamp16(y));
        let r = self
            .conn
            .translate_coordinates(client, self.root, x16, y16)?
            .reply()?;
        let (rdx, rdy) = (r.dst_x as i32 - x, r.dst_y as i32 - y);
        // Captures show the window area only (popups clipped to it), so
        // coordinate input is limited to the same area.
        if matches!(area, HitArea::Window | HitArea::Body) {
            check_in_window(x, y, info)?;
        }
        let mut start = None;
        for (p, _) in self.client_popups(client)?.into_iter().rev() {
            let (Ok(g), Ok(t)) = (
                self.conn.get_geometry(p)?.reply(),
                self.conn
                    .translate_coordinates(client, p, x16, y16)?
                    .reply(),
            ) else {
                continue;
            };
            let (px, py) = (t.dst_x as i32, t.dst_y as i32);
            if (0..g.width as i32).contains(&px) && (0..g.height as i32).contains(&py) {
                // Root rectangle of the popup.
                let popup = (x + rdx - px, y + rdy - py, g.width as i32, g.height as i32);
                start = Some((p, px, py, popup));
                break;
            }
        }
        let (mut win, mut lx, mut ly) = match (start, area) {
            (Some((p, ..)), HitArea::Body) => {
                return Err(xerr(
                    "input_refused",
                    format!("point ({x}, {y}) is covered by the app's popup window {p:#x} (an open menu, dropdown or tooltip); close it (Escape) first"),
                ))
            }
            (Some((p, px, py, popup)), HitArea::Popup(el)) => {
                // The element must be on this popup, not under another
                // popup over it (a submenu over its parent menu).
                let inside = el.0 >= popup.0 - 1
                    && el.1 >= popup.1 - 1
                    && el.0 + el.2 <= popup.0 + popup.2 + 1
                    && el.1 + el.3 <= popup.1 + popup.3 + 1;
                if !inside {
                    return Err(xerr(
                        "input_refused",
                        format!("the menu item at ({x}, {y}) is not on the popup window {p:#x} at that point (another menu covers it, or the item is partly scrolled out); refusing the click"),
                    ));
                }
                (p, px, py)
            }
            (Some((p, px, py, _)), HitArea::Window) => (p, px, py),
            // An item of an open menu must be on one of its popups; a point
            // beside them (an item scrolled out of a long menu) would
            // otherwise hit the main window.
            (None, HitArea::Popup(_)) => {
                return Err(xerr(
                    "input_refused",
                    format!("point ({x}, {y}) is not on any open popup of the app (the menu item may be scrolled out of view); refusing the click"),
                ))
            }
            (None, _) => (client, x, y),
        };
        for _ in 0..32 {
            let t = self
                .conn
                .translate_coordinates(win, win, lx as i16, ly as i16)?
                .reply()?;
            if t.child == NONE {
                break;
            }
            let c = self
                .conn
                .translate_coordinates(win, t.child, lx as i16, ly as i16)?
                .reply()?;
            win = t.child;
            lx = c.dst_x as i32;
            ly = c.dst_y as i32;
        }
        if win != client {
            let owners = self.owners(&[client, win])?;
            if owners[0].is_none() || owners[0].map(|o| o.0) != owners[1].map(|o| o.0) {
                return Err(xerr(
                    "input_refused",
                    format!(
                        "point ({x}, {y}) is over window {win:#x}, which belongs to another X client (embedded or reparented); refusing input"
                    ),
                ));
            }
        }
        Ok(Target {
            win,
            dx: lx - x,
            dy: ly - y,
            rdx,
            rdy,
        })
    }

    fn send(&self, dest: Window, ev: [u8; 32]) -> Result<()> {
        // Empty mask, no propagation: delivered to the client that created
        // `dest`, independent of pointer position, grabs and focus.
        self.conn
            .send_event(false, dest, EventMask::NO_EVENT, ev)
            .map_err(|e| xerr("input_failed", format!("SendEvent failed: {e}")))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn pointer_event(
        &self,
        kind: u8,
        detail: u8,
        time: u32,
        t: &Target,
        x: i32,
        y: i32,
        state: u16,
    ) -> [u8; 32] {
        let (ex, ey) = t.local(x, y);
        let (rx, ry) = t.root(x, y);
        let ev = xproto::ButtonPressEvent {
            response_type: kind,
            detail,
            sequence: 0,
            time,
            root: self.root,
            event: t.win,
            child: NONE,
            root_x: rx,
            root_y: ry,
            event_x: ex,
            event_y: ey,
            state: KeyButMask::from(state),
            same_screen: true,
        };
        // Button and motion events share the same wire layout.
        ev.into()
    }

    fn crossing(&self, kind: u8, time: u32, t: &Target, x: i32, y: i32, state: u16) -> [u8; 32] {
        let (ex, ey) = t.local(x, y);
        let (rx, ry) = t.root(x, y);
        xproto::EnterNotifyEvent {
            response_type: kind,
            detail: xproto::NotifyDetail::NONLINEAR,
            sequence: 0,
            time,
            root: self.root,
            event: t.win,
            child: NONE,
            root_x: rx,
            root_y: ry,
            event_x: ex,
            event_y: ey,
            state: KeyButMask::from(state),
            mode: xproto::NotifyMode::NORMAL,
            same_screen_focus: 0x02,
        }
        .into()
    }

    /// Moves the virtual cursor (enter/leave + motion) and returns the target.
    fn move_to(
        &mut self,
        info: &WindowInfo,
        x: i32,
        y: i32,
        time: u32,
        state: u16,
        area: HitArea,
    ) -> Result<Target> {
        let client = info.window_id;
        let t = self.hit_test(info, x, y, area)?;
        let prev = self.hover.get(&client).copied();
        if prev != Some(t.win) {
            if let Some(old) = prev {
                if let Ok(ot) = self.hit_target_for(client, old) {
                    self.send(
                        old,
                        self.crossing(xproto::LEAVE_NOTIFY_EVENT, time, &ot, x, y, state),
                    )?;
                }
            }
            self.send(
                t.win,
                self.crossing(xproto::ENTER_NOTIFY_EVENT, time, &t, x, y, state),
            )?;
            self.hover.insert(client, t.win);
        }
        self.send(
            t.win,
            self.pointer_event(xproto::MOTION_NOTIFY_EVENT, 0, time, &t, x, y, state),
        )?;
        self.cursors.insert(client, (x, y));
        Ok(t)
    }

    /// Target describing an already known subwindow (for Leave events and grabs).
    fn hit_target_for(&self, client: Window, win: Window) -> Result<Target> {
        let r = self
            .conn
            .translate_coordinates(client, self.root, 0, 0)?
            .reply()?;
        let c = self
            .conn
            .translate_coordinates(client, win, 0, 0)?
            .reply()?;
        Ok(Target {
            win,
            dx: c.dst_x as i32,
            dy: c.dst_y as i32,
            rdx: r.dst_x as i32,
            rdy: r.dst_y as i32,
        })
    }

    /// Pointer position reported in key events: the virtual cursor (or the
    /// window origin) in window-local and root coordinates.
    fn key_origin(&self, client: Window) -> Result<KeyOrigin> {
        let (x, y) = self.cursors.get(&client).copied().unwrap_or((0, 0));
        let r = self
            .conn
            .translate_coordinates(client, self.root, 0, 0)?
            .reply()?;
        Ok(KeyOrigin {
            client,
            ex: clamp16(x),
            ey: clamp16(y),
            rx: clamp16(r.dst_x as i32 + x),
            ry: clamp16(r.dst_y as i32 + y),
        })
    }

    fn key_event(
        &self,
        o: &KeyOrigin,
        press: bool,
        keycode: u8,
        state: u16,
        time: u32,
    ) -> Result<()> {
        let ev = xproto::KeyPressEvent {
            response_type: if press {
                xproto::KEY_PRESS_EVENT
            } else {
                xproto::KEY_RELEASE_EVENT
            },
            detail: keycode,
            sequence: 0,
            time,
            root: self.root,
            event: o.client,
            child: NONE,
            root_x: o.rx,
            root_y: o.ry,
            event_x: o.ex,
            event_y: o.ey,
            state: KeyButMask::from(state),
            same_screen: true,
        };
        self.send(o.client, ev.into())
    }

    fn resolve_keysym(&self, sym: u32) -> Option<(u8, u16)> {
        self.keymap.lookup.get(&sym).copied()
    }

    /// Presses modifier keys (for a chord or a modified pointer action) and
    /// returns the held keycodes with their masks, plus the resulting state.
    fn press_modifiers(
        &mut self,
        o: &KeyOrigin,
        mods: &[Modifier],
        time: &mut u32,
    ) -> Result<(Vec<(u8, u16)>, u16)> {
        let mut held: Vec<(u8, u16)> = Vec::new();
        let mut state = 0u16;
        for &m in mods {
            let (mask, msym) = self.keymap.modifier(m);
            if let Some((kc, _)) = self.resolve_keysym(msym) {
                *time = time.wrapping_add(1);
                self.key_event(o, true, kc, state, *time)?;
                held.push((kc, mask));
            }
            state |= mask;
        }
        Ok((held, state))
    }

    fn release_modifiers(
        &mut self,
        o: &KeyOrigin,
        held: Vec<(u8, u16)>,
        mut state: u16,
        time: &mut u32,
    ) -> Result<()> {
        for (kc, mask) in held.into_iter().rev() {
            *time = time.wrapping_add(1);
            self.key_event(o, false, kc, state, *time)?;
            state &= !mask;
        }
        Ok(())
    }

    /// Keeps a pressed button or key down for `hold`, watching for physical
    /// input. The caller releases in any case, and `time` is resynchronized
    /// with the server afterwards.
    fn hold(&mut self, hold: Duration, time: &mut u32) -> Result<()> {
        if hold.is_zero() {
            return Ok(());
        }
        self.conn.flush()?;
        let end = Instant::now() + hold;
        let r = loop {
            if let Err(e) = self.check_user_input() {
                break Err(e);
            }
            let now = Instant::now();
            if now >= end {
                break Ok(());
            }
            self.wait_readable((end - now).min(Duration::from_millis(20)))?;
        };
        *time = self.server_time()?;
        r
    }

    /// Presses modifiers, the key (held for `hold`), then releases in
    /// reverse order.
    fn send_combo(
        &mut self,
        o: &KeyOrigin,
        mods: &[Modifier],
        sym: u32,
        time: &mut u32,
        hold: Duration,
    ) -> Result<()> {
        let (keycode, level_state) = self.resolve_keysym(sym).ok_or_else(|| {
            xerr(
                "unsupported",
                format!("keysym {sym:#x} is not in the display's keyboard map"),
            )
        })?;
        let o = *o;
        let (held, state) = self.press_modifiers(&o, mods, time)?;
        // Explicit modifiers win; a level shift is added only when needed.
        let key_state = state | level_state;
        *time = time.wrapping_add(1);
        self.key_event(&o, true, keycode, key_state, *time)?;
        let r = self.hold(hold, time);
        *time = time.wrapping_add(1);
        self.key_event(&o, false, keycode, key_state, *time)?;
        self.release_modifiers(&o, held, state, time)?;
        r
    }

    /// Window-bound X11 input for a window the caller already resolved and
    /// authorized (`prepare_action` first). Supported methods: click, drag,
    /// scroll, press_key. Text input and element targets belong to the
    /// accessibility layer and are rejected here.
    pub fn action(&mut self, info: &WindowInfo, method: &str, p: &Value) -> Result<()> {
        let allowed: &[&str] = match method {
            "click" => &[
                "window_id",
                "x",
                "y",
                "button",
                "click_count",
                "modifiers",
                "hold_ms",
            ],
            "drag" => &["window_id", "from", "to", "path", "modifiers"],
            "scroll" => &["window_id", "x", "y", "direction", "pixels", "modifiers"],
            "press_key" => &["window_id", "key", "hold_ms"],
            "mouse_move" => &["window_id", "x", "y", "dx", "dy"],
            other => {
                return Err(xerr(
                    "unsupported",
                    format!("method {other:?} is not an X11 input action"),
                ))
            }
        };
        let obj = p
            .as_object()
            .ok_or_else(|| invalid("params must be an object"))?;
        if obj.contains_key("element_index") {
            return Err(invalid(
                "element_index must be resolved by the accessibility layer; X11 input takes x,y only",
            ));
        }
        if let Some(k) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(invalid(format!("unsupported parameter {k:?} for {method}")));
        }
        if let Some(w) = obj.get("window_id") {
            if w.as_u64() != Some(info.window_id as u64) {
                return Err(invalid("window_id parameter does not match target window"));
            }
        }
        let mut time = self.server_time()?;
        let res = match method {
            "click" => self.do_click(info, p, &mut time, HitArea::Window),
            "drag" => self.do_drag(info, p, &mut time),
            "scroll" => self.do_scroll(info, p, &mut time),
            "press_key" => self.do_key(info, p, &mut time),
            "mouse_move" => self.do_move(info, p, time),
            _ => unreachable!(),
        };
        self.conn.flush()?;
        res
    }

    /// Pointer click at the window-local centre of an accessible element,
    /// which may lie on a popup beyond the window. Used instead of AT-SPI
    /// DoAction where the toolkit would run a nested main loop inside the
    /// accessibility call (GTK menu items opening modal dialogs).
    pub fn click_element_point(
        &mut self,
        info: &WindowInfo,
        click: &Value,
        area: HitArea,
    ) -> Result<()> {
        let mut time = self.server_time()?;
        let r = self.do_click(info, click, &mut time, area);
        self.conn.flush()?;
        r
    }

    /// Root coordinates of a window's origin.
    pub fn origin(&self, win: Window) -> Result<(i32, i32)> {
        let r = self
            .conn
            .translate_coordinates(win, self.root, 0, 0)?
            .reply()?;
        Ok((r.dst_x as i32, r.dst_y as i32))
    }

    /// Types text as synthesized key events to the window (fallback when no
    /// accessible editable text has focus). Every character must be on the
    /// display's keyboard map; nothing is sent otherwise.
    /// Fails, without sending anything, if `text` cannot be typed as key
    /// events on this display.
    pub fn check_typeable(&self, text: &str) -> Result<()> {
        keysyms_for(&self.keymap, text).map(|_| ())
    }

    pub fn type_keys(&mut self, info: &WindowInfo, text: &str) -> Result<()> {
        let syms = keysyms_for(&self.keymap, text)?;
        let o = self.key_origin(info.window_id)?;
        let mut time = self.server_time()?;
        for (i, sym) in syms.into_iter().enumerate() {
            // Physical input aborts long text between characters too.
            if i > 0 && i % 16 == 0 {
                self.conn.flush()?;
                self.check_user_input()?;
            }
            time = time.wrapping_add(2);
            self.send_combo(&o, &[], sym, &mut time, Duration::ZERO)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Moves the virtual cursor (hover) to `x,y`, or by `dx,dy` from where
    /// it is.
    fn do_move(&mut self, info: &WindowInfo, p: &Value, time: u32) -> Result<()> {
        let absolute = p.get("x").is_some() || p.get("y").is_some();
        let relative = p.get("dx").is_some() || p.get("dy").is_some();
        let (x, y) = match (absolute, relative) {
            (true, false) => point(p, "x", "y")?,
            (false, true) => {
                let (dx, dy) = point(p, "dx", "dy")?;
                let Some((cx, cy)) = self.cursors.get(&info.window_id).copied() else {
                    return Err(invalid(
                        "the pointer has no position in this window yet; move to [x, y] first",
                    ));
                };
                (cx + dx, cy + dy)
            }
            _ => return Err(invalid("provide x,y or dx,dy")),
        };
        self.move_to(info, x, y, time, 0, HitArea::Window)?;
        Ok(())
    }

    fn do_click(
        &mut self,
        info: &WindowInfo,
        p: &Value,
        time: &mut u32,
        area: HitArea,
    ) -> Result<()> {
        let (x, y) = point(p, "x", "y")?;
        let button = match p.get("button") {
            None => 1u8,
            Some(v) => match v.as_str() {
                Some("left") => 1,
                Some("middle") => 2,
                Some("right") => 3,
                _ => return Err(invalid("button must be \"left\", \"middle\" or \"right\"")),
            },
        };
        let count = match p.get("click_count") {
            None => 1,
            Some(v) => match int(v) {
                Some(n @ 1..=3) => n as u32,
                _ => return Err(invalid("click_count must be 1, 2 or 3")),
            },
        };
        let mods = parse_modifiers(p.get("modifiers"))?;
        let hold = hold_ms(p)?;
        let mask = 0x80u16 << button; // Button1Mask = 0x100
        let o = self.key_origin(info.window_id)?;
        let (held, state) = self.press_modifiers(&o, &mods, time)?;
        let t = self.move_to(info, x, y, *time, state, area)?;
        let mut r = Ok(());
        for _ in 0..count {
            *time = time.wrapping_add(10);
            self.send(
                t.win,
                self.pointer_event(xproto::BUTTON_PRESS_EVENT, button, *time, &t, x, y, state),
            )?;
            // A held button is released even when physical input aborts.
            r = self.hold(hold, time);
            *time = time.wrapping_add(10);
            self.send(
                t.win,
                self.pointer_event(
                    xproto::BUTTON_RELEASE_EVENT,
                    button,
                    *time,
                    &t,
                    x,
                    y,
                    state | mask,
                ),
            )?;
            if r.is_err() {
                break;
            }
        }
        self.release_modifiers(&o, held, state, time)?;
        r
    }

    fn do_drag(&mut self, info: &WindowInfo, p: &Value, time: &mut u32) -> Result<()> {
        let points = drag_points(p)?;
        let mods = parse_modifiers(p.get("modifiers"))?;
        // Validate the entire path before pressing, including intermediate
        // segment samples: refuse points covered by another client before input.
        let mut motions = Vec::new();
        for segment in points.windows(2) {
            let (x0, y0) = segment[0];
            let (x1, y1) = segment[1];
            for i in 1..=DRAG_STEPS {
                let point = (
                    x0 + (x1 - x0) * i / DRAG_STEPS,
                    y0 + (y1 - y0) * i / DRAG_STEPS,
                );
                self.hit_test(info, point.0, point.1, HitArea::Window)?;
                motions.push((point.0, point.1, i == DRAG_STEPS));
            }
        }
        let (x0, y0) = points[0];
        let o = self.key_origin(info.window_id)?;
        let (held, mstate) = self.press_modifiers(&o, &mods, time)?;
        let b1: u16 = 0x100 | mstate;
        let t = self.move_to(info, x0, y0, *time, mstate, HitArea::Window)?;
        *time = time.wrapping_add(1);
        self.send(
            t.win,
            self.pointer_event(xproto::BUTTON_PRESS_EVENT, 1, *time, &t, x0, y0, mstate),
        )?;
        self.conn.flush()?;
        // Implicit grab semantics: motion and release go to the press window.
        let (mut cx, mut cy) = (x0, y0);
        let mut aborted = None;
        // These samples are sent in a burst, not 16 ms apart. Fabricating a
        // frame per sample put a long stroke seconds ahead of server time;
        // the next request then went backwards in time. Twelve 1 ms ticks
        // per segment fit within the vertex's real 20 ms delivery interval.
        for (x, y, vertex) in motions {
            cx = x;
            cy = y;
            *time = time.wrapping_add(1);
            let step = (|| -> Result<()> {
                self.send(
                    t.win,
                    self.pointer_event(xproto::MOTION_NOTIFY_EVENT, 0, *time, &t, cx, cy, b1),
                )?;
                self.conn.flush()?;
                // GTK coalesces queued motion events to the last position.
                // Give each requested vertex one frame to be consumed; without
                // this a closed pencil stroke collapses to a dot, and a
                // two-point line may release before its motion is consumed.
                if vertex {
                    std::thread::sleep(Duration::from_millis(20));
                }
                self.check_user_input()
            })();
            if let Err(e) = step {
                aborted = Some(e);
                break;
            }
        }
        *time = time.wrapping_add(1);
        self.send(
            t.win,
            self.pointer_event(xproto::BUTTON_RELEASE_EVENT, 1, *time, &t, cx, cy, b1),
        )?;
        self.cursors.insert(info.window_id, (cx, cy));
        self.release_modifiers(&o, held, mstate, time)?;
        match aborted {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn do_scroll(&mut self, info: &WindowInfo, p: &Value, time: &mut u32) -> Result<()> {
        let (x, y) = point(p, "x", "y")?;
        let mods = parse_modifiers(p.get("modifiers"))?;
        let button = match p.get("direction").and_then(Value::as_str) {
            Some("up") => 4u8,
            Some("down") => 5,
            Some("left") => 6,
            Some("right") => 7,
            _ => return Err(invalid("direction must be up, down, left or right")),
        };
        let pixels = p
            .get("pixels")
            .and_then(int)
            .filter(|&n| n > 0 && n <= (SCROLL_PIXELS_PER_NOTCH * MAX_SCROLL_NOTCHES) as i64)
            .ok_or_else(|| {
                invalid(format!(
                    "pixels must be an integer in 1..={}",
                    SCROLL_PIXELS_PER_NOTCH * MAX_SCROLL_NOTCHES
                ))
            })? as u32;
        let notches = pixels.div_ceil(SCROLL_PIXELS_PER_NOTCH);
        let o = self.key_origin(info.window_id)?;
        let (held, state) = self.press_modifiers(&o, &mods, time)?;
        let t = self.move_to(info, x, y, *time, state, HitArea::Window)?;
        let release_mask = (if button <= 5 { 0x80u16 << button } else { 0 }) | state;
        for _ in 0..notches {
            *time = time.wrapping_add(5);
            self.send(
                t.win,
                self.pointer_event(xproto::BUTTON_PRESS_EVENT, button, *time, &t, x, y, state),
            )?;
            self.send(
                t.win,
                self.pointer_event(
                    xproto::BUTTON_RELEASE_EVENT,
                    button,
                    *time,
                    &t,
                    x,
                    y,
                    release_mask,
                ),
            )?;
        }
        self.release_modifiers(&o, held, state, time)
    }

    fn do_key(&mut self, info: &WindowInfo, p: &Value, time: &mut u32) -> Result<()> {
        let key = p
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("key must be a string such as \"Return\" or \"ctrl+s\""))?;
        let (mods, sym) = parse_key_combo(key)?;
        let hold = hold_ms(p)?;
        if self.resolve_keysym(sym).is_none() {
            return Err(xerr(
                "unsupported",
                format!("key {key:?} is not in the display keyboard map"),
            ));
        }
        let o = self.key_origin(info.window_id)?;
        self.send_combo(&o, &mods, sym, time, hold)
    }
}

/// `modifiers: ["shift", "ctrl", ...]` held during a pointer action.
fn parse_modifiers(v: Option<&Value>) -> Result<Vec<Modifier>> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let arr = v.as_array().filter(|a| a.len() <= 4).ok_or_else(|| {
        invalid("modifiers must be an array of at most 4 of shift, ctrl, alt, super")
    })?;
    let mut out = Vec::new();
    for m in arr {
        let m = parse_modifier(
            m.as_str()
                .ok_or_else(|| invalid("modifiers must be strings such as \"shift\""))?,
        )?;
        if !out.contains(&m) {
            out.push(m);
        }
    }
    Ok(out)
}

/// `hold_ms`: how long a button or key stays pressed.
fn hold_ms(p: &Value) -> Result<Duration> {
    match p.get("hold_ms") {
        None => Ok(Duration::ZERO),
        Some(v) => int(v)
            .filter(|n| (0..=MAX_HOLD_MS).contains(n))
            .map(|n| Duration::from_millis(n as u64))
            .ok_or_else(|| invalid(format!("hold_ms must be an integer in 0..={MAX_HOLD_MS}"))),
    }
}

// ---------------------------------------------------------------------------
// Parameter helpers
// ---------------------------------------------------------------------------

fn int(v: &Value) -> Option<i64> {
    if let Some(i) = v.as_i64() {
        return Some(i);
    }
    v.as_f64()
        .filter(|f| f.fract() == 0.0 && f.abs() < 1e9)
        .map(|f| f as i64)
}

fn check_in_window(x: i32, y: i32, info: &WindowInfo) -> Result<()> {
    if x < 0 || y < 0 || x >= info.width as i32 || y >= info.height as i32 {
        return Err(invalid(format!(
            "point ({x}, {y}) is outside window {} ({}x{}) and its popups",
            info.window_id, info.width, info.height
        )));
    }
    Ok(())
}

/// Window-local coordinate. Bounds are checked by hit-testing, because
/// popups may extend beyond the window.
fn coord(v: Option<&Value>, name: &str) -> Result<i32> {
    v.and_then(int)
        .filter(|n| (i16::MIN as i64..=i16::MAX as i64).contains(n))
        .map(|n| n as i32)
        .ok_or_else(|| invalid(format!("{name} must be an integer")))
}

fn point(p: &Value, kx: &str, ky: &str) -> Result<(i32, i32)> {
    Ok((coord(p.get(kx), kx)?, coord(p.get(ky), ky)?))
}

fn pair(p: &Value, key: &str) -> Result<(i32, i32)> {
    let arr = p
        .get(key)
        .and_then(Value::as_array)
        .filter(|a| a.len() == 2)
        .ok_or_else(|| invalid(format!("{key} must be [x, y]")))?;
    Ok((coord(arr.first(), key)?, coord(arr.get(1), key)?))
}

/// A bounded continuous stroke, or the original two-point drag. Reject the
/// whole request before input when any point or the combination is invalid.
fn drag_points(p: &Value) -> Result<Vec<(i32, i32)>> {
    if let Some(path) = p.get("path") {
        if p.get("from").is_some() || p.get("to").is_some() {
            return Err(invalid("provide path or from/to, not both"));
        }
        let path = path
            .as_array()
            .filter(|v| (2..=256).contains(&v.len()))
            .ok_or_else(|| invalid("path must contain 2..=256 [x, y] points"))?;
        path.iter()
            .map(|v| pair(&json!({"point": v}), "point"))
            .collect()
    } else {
        Ok(vec![pair(p, "from")?, pair(p, "to")?])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opt-in, on a private X server (never `:0`, which `connect` refuses):
    /// `CUA_X11_TEST_DISPLAY=:78 cargo test --locked x11::tests::clipboard`.
    /// Skipped (passes) without the variable, so the gate stays hermetic.
    #[test]
    fn clipboard_read_authorizes_the_owner_and_notices_a_new_one() {
        let Ok(display) = std::env::var("CUA_X11_TEST_DISPLAY") else {
            return;
        };
        let mut d = Desktop::connect(&display).unwrap();
        let clip = d.atoms.CLIPBOARD;
        let code = |r: Result<Option<String>>| {
            r.unwrap_err()
                .downcast_ref::<X11Error>()
                .map(|e| e.code)
                .unwrap_or("other")
        };
        // A client with a listed window (later two, of different apps) that
        // owns the clipboard directly, through an unlisted helper window, or
        // through a second connection of the same process.
        let (app, _) = x11rb::connect(Some(&display)).unwrap();
        let root = app.setup().roots[0].root;
        let window = |c: &RustConnection, class: Option<&[u8]>, map: bool| {
            let w = c.generate_id().unwrap();
            c.create_window(
                0,
                w,
                root,
                0,
                0,
                50,
                50,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap();
            if let Some(class) = class {
                c.change_property8(
                    PropMode::REPLACE,
                    w,
                    AtomEnum::WM_CLASS,
                    AtomEnum::STRING,
                    class,
                )
                .unwrap();
            }
            if map {
                c.map_window(w).unwrap();
            }
            c.get_input_focus().unwrap().reply().unwrap();
            w
        };
        let a = window(&app, Some(b"alpha\0Alpha\0"), true);
        let own = |c: &RustConnection, w: Window| {
            c.set_selection_owner(w, clip, x11rb::CURRENT_TIME).unwrap();
            c.get_input_focus().unwrap().reply().unwrap();
        };
        let seen = std::cell::RefCell::new(Vec::new());
        let deny = |w: &WindowInfo| {
            seen.borrow_mut().push(w.app_id.clone());
            Err(xerr("app_denied", "denied by the test"))
        };
        // The exact owner window is the one authorized.
        own(&app, a);
        assert_eq!(code(d.clipboard_read(deny)), "app_denied");
        assert_eq!(seen.take(), vec!["Alpha".to_string()]);
        // An unlisted helper window of that client: the client's app.
        let helper = window(&app, None, false);
        own(&app, helper);
        assert_eq!(code(d.clipboard_read(deny)), "app_denied");
        assert_eq!(seen.take(), vec!["Alpha".to_string()]);
        // A second connection of the same process: the process's app.
        let (other, _) = x11rb::connect(Some(&display)).unwrap();
        let unlisted = window(&other, None, false);
        own(&other, unlisted);
        assert_eq!(code(d.clipboard_read(deny)), "app_denied");
        assert_eq!(seen.take(), vec!["Alpha".to_string()]);
        // ... unless its windows belong to two apps.
        window(&app, Some(b"beta\0Beta\0"), true);
        for owner in [(&app, helper), (&other, unlisted)] {
            own(owner.0, owner.1);
            assert_eq!(code(d.clipboard_read(deny)), "app_denied");
            assert!(
                seen.take().is_empty(),
                "an ambiguous owner is not authorized"
            );
        }
        // A new owner after the check aborts the read.
        own(&app, a);
        let switch = |_: &WindowInfo| {
            own(&other, unlisted);
            Ok(())
        };
        assert_eq!(code(d.clipboard_read(switch)), "clipboard_changed");
    }

    #[test]
    fn continuous_drag_validation() {
        assert_eq!(
            drag_points(&json!({"path": [[1,2], [9,2], [9,8], [1,2]]})).unwrap(),
            vec![(1, 2), (9, 2), (9, 8), (1, 2)]
        );
        assert_eq!(
            drag_points(&json!({"from": [1,2], "to": [3,4]})).unwrap(),
            vec![(1, 2), (3, 4)]
        );
        for p in [
            json!({"path": []}),
            json!({"path": [[1,2]]}),
            json!({"path": [[1,2], [3,4,5]]}),
            json!({"path": [[1,2], [3.5,4]]}),
            json!({"path": [[1,2], [3,4]], "from": [1,2]}),
            json!({"path": vec![[1,2]; 257]}),
        ] {
            assert!(drag_points(&p).is_err(), "{p}");
        }
    }

    #[test]
    fn modifier_and_hold_params() {
        assert_eq!(
            parse_modifiers(Some(&json!(["shift", "Ctrl", "shift"]))).unwrap(),
            vec![Modifier::Shift, Modifier::Ctrl]
        );
        assert_eq!(parse_modifiers(None).unwrap(), vec![]);
        for v in [
            json!("shift"),
            json!(["cmd"]),
            json!([1]),
            json!(["a", "b", "c", "d", "e"]),
        ] {
            assert!(parse_modifiers(Some(&v)).is_err(), "{v}");
        }
        assert_eq!(hold_ms(&json!({})).unwrap(), Duration::ZERO);
        assert_eq!(
            hold_ms(&json!({"hold_ms": 250})).unwrap(),
            Duration::from_millis(250)
        );
        for v in [
            json!({"hold_ms": -1}),
            json!({"hold_ms": 10_001}),
            json!({"hold_ms": "1"}),
        ] {
            assert!(hold_ms(&v).is_err(), "{v}");
        }
    }

    #[test]
    fn display_zero_denied() {
        for d in [
            ":0",
            ":0.0",
            "unix:0",
            "localhost:0.1",
            "/tmp/.X11-unix/X0",
            "",
            "garbage",
            ":x",
        ] {
            assert!(check_display_allowed(d).is_err(), "{d}");
        }
        for d in [":1", ":99.0", "unix:7", "/tmp/.X11-unix/X5"] {
            assert!(check_display_allowed(d).is_ok(), "{d}");
        }
        let e = Desktop::connect(":0").err().unwrap();
        assert_eq!(e.downcast_ref::<X11Error>().unwrap().code, "display_denied");
    }

    /// Regression: the close wait ran to its 1 s cap for every confirm key
    /// in a dialog that stays open. Measured on the GTK file chooser: a
    /// dialog that closes shows no activity for ~400 ms before unmapping; one
    /// that stays open (navigating into a folder) shows a burst, then quiet.
    #[test]
    fn dialog_close_wait_ends_for_dialogs_that_stay_open() {
        let ms = Duration::from_millis;
        let (idle, still, max) = (ms(175), ms(500), ms(900));
        // Stays open: activity until 390 ms, quiet since -> over at ~565 ms.
        assert!(close_wait_over(true, ms(180), ms(570), idle, still, max));
        assert!(!close_wait_over(true, ms(100), ms(490), idle, still, max));
        // Closing without any activity first: keep waiting past 420 ms.
        assert!(!close_wait_over(false, ms(0), ms(420), idle, still, max));
        assert!(close_wait_over(false, ms(0), ms(500), idle, still, max));
        assert!(close_wait_over(true, ms(0), ms(900), idle, still, max));
    }

    #[test]
    fn key_combos() {
        assert_eq!(parse_key_combo("Return").unwrap(), (vec![], XK_RETURN));
        assert_eq!(
            parse_key_combo("ctrl+shift+T").unwrap(),
            (vec![Modifier::Ctrl, Modifier::Shift], 't' as u32)
        );
        assert_eq!(
            parse_key_combo("ctrl++").unwrap(),
            (vec![Modifier::Ctrl], '+' as u32)
        );
        assert_eq!(parse_key_combo("+").unwrap(), (vec![], '+' as u32));
        assert_eq!(parse_key_combo("F12").unwrap().1, 0xffbe + 11);
        // GPT-style spellings: any case, Enter for Return, arrow names; a
        // lone capital letter keeps its case, with modifiers it names the key.
        for k in ["Ctrl+s", "ctrl+S", "CTRL+s", "Control+s"] {
            assert_eq!(
                parse_key_combo(k).unwrap(),
                (vec![Modifier::Ctrl], 's' as u32),
                "{k}"
            );
        }
        for k in ["Enter", "ENTER", "return"] {
            assert_eq!(parse_key_combo(k).unwrap(), (vec![], XK_RETURN), "{k}");
        }
        assert_eq!(parse_key_combo("ArrowDown").unwrap().1, 0xff54);
        assert_eq!(parse_key_combo("A").unwrap().1, 'A' as u32);
        assert!(parse_key_combo("cmd+c").is_err());
        assert!(parse_key_combo("hyper+x").is_err());
        assert!(parse_key_combo("NotAKey").is_err());
        assert!(parse_key_combo("ctrl+").is_err());
        assert!(parse_key_combo("F36").is_err());
    }

    #[test]
    fn keymap_levels_and_modifiers() {
        // keycodes 8..: 8 = a/A, 9 = 1/!, 10 = q (implicit Q), 11 = Shift_L, 12 = ISO_Level3_Shift, 13 = e/E/€
        let per = 6;
        let mut syms = vec![0u32; 6 * per];
        syms[0..2].copy_from_slice(&[0x61, 0x41]);
        syms[per..per + 2].copy_from_slice(&[0x31, 0x21]);
        syms[2 * per] = 0x71;
        syms[3 * per] = XK_SHIFT_L;
        syms[4 * per] = XK_ISO_LEVEL3_SHIFT;
        syms[5 * per..5 * per + 6].copy_from_slice(&[0x65, 0x45, 0, 0, 0x20ac, 0]);
        let mut modmap = vec![0u8; 8];
        modmap[0] = 11; // shift
        modmap[7] = 12; // mod5
        let km = Keymap::build(8, per, &syms, &modmap);
        assert_eq!(km.lookup[&0x61], (8, 0));
        assert_eq!(km.lookup[&0x41], (8, SHIFT));
        assert_eq!(km.lookup[&0x21], (9, SHIFT));
        assert_eq!(km.lookup[&0x51], (10, SHIFT));
        assert_eq!(km.lookup[&0x20ac], (13, MOD5));
        assert_eq!(km.level3_mask, MOD5);
        assert!(!km.lookup.contains_key(&0x7a));
        // Typing falls back to the legacy EuroSign keysym the map uses.
        assert_eq!(keysyms_for(&km, "a€").unwrap(), vec![0x61, 0x20ac]);
        assert!(keysyms_for(&km, "az").is_err());
    }

    #[test]
    fn pixel_conversion() {
        let l = PixelLayout {
            bpp: 32,
            stride: 8,
            lsb_first: true,
            red: 0xff0000,
            green: 0xff00,
            blue: 0xff,
        };
        let data = [3, 2, 1, 0, 30, 20, 10, 0];
        assert_eq!(to_rgb(&data, 2, 1, &l).unwrap(), vec![1, 2, 3, 10, 20, 30]);
        let l16 = PixelLayout {
            bpp: 16,
            stride: 2,
            lsb_first: true,
            red: 0xf800,
            green: 0x07e0,
            blue: 0x001f,
        };
        assert_eq!(
            to_rgb(&0xffffu16.to_le_bytes(), 1, 1, &l16).unwrap(),
            vec![255, 255, 255]
        );
        let lbe = PixelLayout {
            lsb_first: false,
            ..l
        };
        assert_eq!(to_rgb(&[0, 1, 2, 3], 1, 1, &lbe).unwrap(), vec![1, 2, 3]);
        assert!(to_rgb(&[0; 4], 2, 1, &l).is_err());
        let png = encode_png(&[1, 2, 3, 10, 20, 30], 2, 1).unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn param_validation() {
        let info = WindowInfo {
            window_id: 5,
            title: String::new(),
            app_id: "a".into(),
            pid: 0,
            width: 100,
            height: 50,
            transient_for: None,
            origin: (0, 0),
        };
        assert_eq!(
            point(&json!({"x": 10, "y": 49}), "x", "y").unwrap(),
            (10, 49)
        );
        assert!(check_in_window(10, 49, &info).is_ok());
        assert!(check_in_window(100, 0, &info).is_err());
        assert!(check_in_window(-1, 0, &info).is_err());
        // Negative coordinates parse (popups can extend left of the window).
        assert_eq!(point(&json!({"x": -1, "y": 0}), "x", "y").unwrap(), (-1, 0));
        assert!(point(&json!({"x": 1.5, "y": 0}), "x", "y").is_err());
        assert!(point(&json!({"x": 40000, "y": 0}), "x", "y").is_err());
        assert_eq!(pair(&json!({"from": [1.0, 2]}), "from").unwrap(), (1, 2));
        assert!(pair(&json!({"from": [1]}), "from").is_err());
        assert_eq!(char_keysym('é'), Some(0xe9));
        assert_eq!(char_keysym('ж'), Some(0x0100_0436));
        assert_eq!(char_keysym('\u{7}'), None);
    }

    #[test]
    fn popup_blit_clips_to_window() {
        // 3x2 black window, 2x2 white popup at (2, -1): only (2, 0) changes.
        let mut dst = vec![0u8; 3 * 2 * 3];
        blit(&mut dst, (3, 2), &[255u8; 2 * 2 * 3], (2, 2), (2, -1));
        let white: Vec<usize> = (0..6).filter(|i| dst[i * 3] == 255).collect();
        assert_eq!(white, vec![2]);
        blit(&mut dst, (3, 2), &[7u8; 3], (1, 1), (-5, 0));
        assert!(!dst.contains(&7));
    }
}
