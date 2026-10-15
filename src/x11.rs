//! X11 backend: window enumeration, window-bound background input, Composite
//! capture, XI2 raw-input abort latch and XDamage-based settling.
//!
//! Clean-room implementation on top of x11rb. Design rules:
//! * Never connect to display 0.
//! * Never use XTEST, never warp the pointer, never change input focus, never
//!   grab. Input is delivered with `SendEvent` to the creating client of the
//!   target window and positioned with a per-window virtual cursor.
//! * Only windows that `list_windows` reports are valid targets.
//! * Physical input (anything the server reports as an XI2 raw event, which
//!   synthetic `SendEvent` input never produces) latches an abort until
//!   `clear_input_abort`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::Serialize;
use serde_json::{json, Value};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::composite::{self, ConnectionExt as _};
use x11rb::protocol::damage::{self, ConnectionExt as _};
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
use x11rb::{CURRENT_TIME, NONE};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        _NET_CLIENT_LIST,
        _NET_WM_NAME,
        _NET_WM_PID,
        UTF8_STRING,
        WM_STATE,
        CLIPBOARD,
        TARGETS,
        TEXT,
        _LINUX_CUA_TIME,
    }
}

/// Window description shared with the daemon and policy layer.
#[derive(Serialize, Clone, Debug)]
pub struct WindowInfo {
    pub window_id: u32,
    pub title: String,
    pub app_id: String,
    pub pid: u32,
    pub width: u16,
    pub height: u16,
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
const MAX_TEXT_CHARS: usize = 20_000;
const DRAG_STEPS: i32 = 12;
const PASTE_CONSUME_TIMEOUT: Duration = Duration::from_millis(2000);
const CLIPBOARD_LIFETIME: Duration = Duration::from_secs(10);

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

/// Legacy (non-Unicode-range) keysyms some keymaps use for common characters.
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
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "space" => 0x20,
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
        let c = first.to_ascii_lowercase();
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

// The mapping is only accessed while holding the Desktop's state mutex.
unsafe impl Send for ShmBuf {}

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

#[derive(Default)]
struct Shared {
    shm: Option<ShmBuf>,
    shm_disabled: bool,
    redirected: HashSet<Window>,
    damage: HashMap<Window, damage::Damage>,
}

struct Clipboard {
    text: String,
    expires: Instant,
    served: u32,
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

pub struct Desktop {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    helper: Window,
    keymap: Keymap,
    shm_available: bool,
    state: Mutex<Shared>,
    abort: Option<String>,
    cursors: HashMap<Window, (i32, i32)>,
    hover: HashMap<Window, Window>,
    settle_target: Option<Window>,
    popups: HashSet<Window>,
    clipboard: Option<Clipboard>,
    authorized_windows: Option<(HashSet<Window>, u32)>,
}

impl Desktop {
    pub fn connect(display: &str) -> Result<Self> {
        check_display_allowed(display)?;
        Self::connect_inner(display, None)
    }

    /// Explicit trusted-host consent for specific windows of one process.
    /// Unlike normal connection, this may target display 0, but cannot discover
    /// or control any other windows. X11 client metadata is not tamper-proof.
    pub fn connect_authorized(display: &str, windows: Vec<u32>, pid: u32) -> Result<Self> {
        display_number(display)?;
        if windows.is_empty() || windows.contains(&0) || pid == 0 {
            return Err(invalid(
                "explicit window IDs and a nonzero process ID are required",
            ));
        }
        Self::connect_inner(display, Some((windows.into_iter().collect(), pid)))
    }

    fn connect_inner(
        display: &str,
        authorized_windows: Option<(HashSet<Window>, u32)>,
    ) -> Result<Self> {
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

        // Invisible helper: timestamps and clipboard ownership. Never mapped.
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

        let keymap = Keymap::load(&conn)?;
        conn.flush()?;

        Ok(Desktop {
            conn,
            root,
            atoms,
            helper,
            keymap,
            shm_available,
            state: Mutex::new(Shared::default()),
            abort: None,
            cursors: HashMap::new(),
            hover: HashMap::new(),
            settle_target: None,
            popups: HashSet::new(),
            clipboard: None,
            authorized_windows,
        })
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
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
        if let Some((windows, _)) = &self.authorized_windows {
            return Ok(windows
                .iter()
                .copied()
                .filter(|w| *w != self.root && *w != self.helper)
                .collect());
        }
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
                self.conn
                    .get_property(false, w, a._NET_WM_PID, AtomEnum::CARDINAL, 0, 1)?,
            ));
        }
        let mut out = Vec::new();
        for (w, attrs, geom, class, net_name, name, pid) in pending {
            let (Ok(attrs), Ok(geom)) = (attrs.reply(), geom.reply()) else {
                let _ = (class.reply(), net_name.reply(), name.reply(), pid.reply());
                continue;
            };
            let class = class.reply().ok();
            let net_name = net_name.reply().ok();
            let name = name.reply().ok();
            let pid = pid.reply().ok();
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
            let pid = pid
                .and_then(|r| r.value32().and_then(|mut v| v.next()))
                .unwrap_or(0);
            if self
                .authorized_windows
                .as_ref()
                .is_some_and(|(_, expected)| pid != *expected)
            {
                continue;
            }
            out.push(WindowInfo {
                window_id: w,
                title,
                app_id,
                pid,
                width: geom.width,
                height: geom.height,
            });
        }
        Ok(out)
    }

    pub fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let wins = self.candidates()?;
        let infos = self.describe(&wins)?;
        // Drop damage objects for windows that are gone or no longer listed.
        let live: HashSet<Window> = infos.iter().map(|i| i.window_id).collect();
        let mut st = self.shared();
        let stale: Vec<Window> = st
            .damage
            .keys()
            .copied()
            .filter(|w| !live.contains(w) && !self.popups.contains(w) && *w != self.root)
            .collect();
        for w in stale {
            if let Some(d) = st.damage.remove(&w) {
                let _ = self.conn.damage_destroy(d);
            }
            st.redirected.remove(&w);
        }
        drop(st);
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

    // ----- capture ---------------------------------------------------------

    fn ensure_damage(&self, win: Window) {
        let mut st = self.shared();
        if st.damage.contains_key(&win) {
            return;
        }
        if let Ok(id) = self.conn.generate_id() {
            if self
                .conn
                .damage_create(id, win, damage::ReportLevel::NON_EMPTY)
                .is_ok()
            {
                st.damage.insert(win, id);
            }
        }
    }

    fn ensure_redirect(&self, win: Window) {
        let mut st = self.shared();
        if st.redirected.insert(win) {
            // BadAccess means another client (a compositor) owns a manual
            // redirection; the window then already has a backing pixmap.
            if let Ok(c) = self
                .conn
                .composite_redirect_window(win, composite::Redirect::AUTOMATIC)
            {
                let _ = c.check();
            }
        }
    }

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

    /// Reads a drawable as RGB, preferring MIT-SHM and falling back to core
    /// GetImage on any SHM failure.
    fn read_drawable(
        &self,
        drawable: u32,
        w: u16,
        h: u16,
        layout: &PixelLayout,
    ) -> Result<Vec<u8>> {
        let needed = layout.stride * h as usize;
        if self.shm_available {
            let mut st = self.shared();
            if !st.shm_disabled {
                if st.shm.as_ref().map(|b| b.len < needed).unwrap_or(true) {
                    if let Some(old) = st.shm.take() {
                        let _ = self.conn.shm_detach(old.seg);
                    }
                    let len = needed.div_ceil(1 << 20) * (1 << 20);
                    match ShmBuf::create(&self.conn, len) {
                        Ok(b) => st.shm = Some(b),
                        Err(_) => st.shm_disabled = true,
                    }
                }
                if let Some(buf) = st.shm.as_ref() {
                    let r = self
                        .conn
                        .shm_get_image(
                            drawable,
                            0,
                            0,
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
        }
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, drawable, 0, 0, w, h, !0)?
            .reply()
            .map_err(|e| xerr("capture_failed", format!("GetImage failed: {e}")))?;
        to_rgb(&reply.data, w as usize, h as usize, layout)
    }

    fn capture(&self, win: Window) -> Result<(u16, u16, Vec<u8>)> {
        self.ensure_redirect(win);
        self.ensure_damage(win);
        let attrs = self.conn.get_window_attributes(win)?.reply()?;
        let pixmap = self.conn.generate_id()?;
        let named = self
            .conn
            .composite_name_window_pixmap(win, pixmap)?
            .check()
            .is_ok();
        if !named {
            return Err(xerr(
                "capture_failed",
                "Composite backing pixmap unavailable; refusing an occlusion-sensitive capture",
            ));
        }
        let drawable = pixmap;
        let result = (|| {
            let geom = self.conn.get_geometry(drawable)?.reply().map_err(|e| {
                xerr(
                    "capture_failed",
                    format!("window geometry unavailable: {e}"),
                )
            })?;
            let layout = self.layout_for(geom.depth, geom.width, attrs.visual)?;
            let rgb = self.read_drawable(drawable, geom.width, geom.height, &layout)?;
            Ok((geom.width, geom.height, rgb))
        })();
        if named {
            let _ = self.conn.free_pixmap(pixmap);
            let _ = self.conn.flush();
        }
        result
    }

    pub fn screenshot(&self, id: u32) -> Result<Value> {
        self.window(id)?;
        let (w, h, rgb) = self.capture(id)?;
        let png = encode_png(&rgb, w, h)?;
        Ok(json!({
            "data": base64::engine::general_purpose::STANDARD.encode(png),
            "mimeType": "image/png",
            "width": w,
            "height": h,
        }))
    }

    // ----- event pump ------------------------------------------------------

    fn latch(&mut self, what: String) {
        if self.abort.is_none() {
            self.abort = Some(what);
        }
    }

    fn relevant_drawable(&self, d: Window) -> bool {
        match self.settle_target {
            Some(t) => d == t || self.popups.contains(&d),
            None => true,
        }
    }

    /// Handles one event; returns whether it counts as activity for settling.
    fn handle(&mut self, ev: Event) -> Result<bool> {
        Ok(match ev {
            Event::XinputRawKeyPress(e) => {
                self.latch(format!("physical key press from device {}", e.sourceid));
                false
            }
            Event::XinputRawButtonPress(e) => {
                self.latch(format!("physical button press from device {}", e.sourceid));
                false
            }
            Event::XinputRawMotion(e) => {
                self.latch(format!(
                    "physical pointer motion from device {}",
                    e.sourceid
                ));
                false
            }
            Event::XinputRawTouchBegin(e) => {
                self.latch(format!("physical touch from device {}", e.sourceid));
                false
            }
            Event::DamageNotify(e) => {
                let _ = self.conn.damage_subtract(e.damage, NONE, NONE);
                self.relevant_drawable(e.drawable)
            }
            Event::MapNotify(e) if e.event == self.root => {
                if e.override_redirect {
                    self.popups.insert(e.window);
                    self.ensure_damage(e.window);
                }
                true
            }
            Event::UnmapNotify(e) if e.event == self.root => {
                let was_popup = self.popups.remove(&e.window);
                if was_popup {
                    if let Some(d) = self.shared().damage.remove(&e.window) {
                        let _ = self.conn.damage_destroy(d);
                    }
                }
                was_popup || self.settle_target.is_none()
            }
            Event::DestroyNotify(e) if e.event == self.root => {
                let was_popup = self.popups.remove(&e.window);
                self.shared().damage.remove(&e.window);
                was_popup
            }
            Event::MappingNotify(e) => {
                if e.request != xproto::Mapping::POINTER {
                    self.keymap = Keymap::load(&self.conn)?;
                }
                false
            }
            Event::SelectionRequest(e) => {
                self.serve_selection(e)?;
                false
            }
            Event::SelectionClear(e) => {
                if e.selection == self.atoms.CLIPBOARD {
                    self.clipboard = None;
                }
                false
            }
            // Asynchronous errors from unchecked requests on vanished windows.
            Event::Error(_) => false,
            _ => false,
        })
    }

    fn pump(&mut self) -> Result<bool> {
        let mut activity = false;
        while let Some(ev) = self.conn.poll_for_event()? {
            activity |= self.handle(ev)?;
        }
        if let Some(c) = &self.clipboard {
            if Instant::now() >= c.expires {
                self.clipboard = None;
                let owner = self
                    .conn
                    .get_selection_owner(self.atoms.CLIPBOARD)?
                    .reply()?
                    .owner;
                if owner == self.helper {
                    self.conn
                        .set_selection_owner(NONE, self.atoms.CLIPBOARD, CURRENT_TIME)?;
                }
            }
        }
        self.conn.flush()?;
        Ok(activity)
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

    /// Round trip so that everything sent so far has been processed.
    fn sync(&self) -> Result<()> {
        self.conn.get_input_focus()?.reply()?;
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
            let ev = self.conn.wait_for_event()?;
            if let Event::PropertyNotify(e) = &ev {
                if e.window == self.helper && e.atom == self.atoms._LINUX_CUA_TIME {
                    return Ok(e.time);
                }
            }
            self.handle(ev)?;
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

    /// Arm damage monitoring before semantic actions as well as X11 input.
    pub fn prepare_action(&mut self, id: u32) -> Result<()> {
        self.check_user_input()?;
        self.window(id)?;
        self.ensure_damage(id);
        self.settle_target = Some(id);
        self.conn.flush()?;
        Ok(())
    }

    /// Waits until the last action's window (and popups it opened) produced
    /// no damage for `quiet`, bounded by `cap`. Returns false on timeout or
    /// when user input latched an abort while waiting.
    pub fn settle(&mut self, quiet: Duration, cap: Duration) -> Result<bool> {
        self.sync()?;
        let start = Instant::now();
        let mut last = start;
        loop {
            if self.pump()? {
                last = Instant::now();
            }
            if self.abort.is_some() {
                return Ok(false);
            }
            let now = Instant::now();
            let quiet_for = now - last;
            let elapsed = now - start;
            if quiet_for >= quiet {
                return Ok(true);
            }
            if elapsed >= cap {
                return Ok(false);
            }
            self.wait_readable((quiet - quiet_for).min(cap - elapsed))?;
        }
    }

    // ----- input -----------------------------------------------------------

    /// Current virtual cursor position (window-local) for a window, if any
    /// pointer action has targeted it.
    pub fn virtual_cursor(&self, id: u32) -> Option<(i32, i32)> {
        self.cursors.get(&id).copied()
    }

    fn ensure_no_abort(&mut self) -> Result<()> {
        self.pump()?;
        if let Some(what) = &self.abort {
            return Err(xerr(
                "user_input_abort",
                format!("user input detected: {what}"),
            ));
        }
        Ok(())
    }

    /// Finds the deepest mapped subwindow of `client` at a window-local point.
    fn hit_test(&self, client: Window, x: i32, y: i32) -> Result<Target> {
        let r = self
            .conn
            .translate_coordinates(client, self.root, x as i16, y as i16)?
            .reply()?;
        let (rdx, rdy) = (r.dst_x as i32 - x, r.dst_y as i32 - y);
        let (mut win, mut lx, mut ly) = (client, x, y);
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
    fn move_to(&mut self, client: Window, x: i32, y: i32, time: u32, state: u16) -> Result<Target> {
        let t = self.hit_test(client, x, y)?;
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

    /// Presses modifiers, the key, then releases in reverse order.
    fn send_combo(
        &mut self,
        client: Window,
        mods: &[Modifier],
        sym: u32,
        time: &mut u32,
    ) -> Result<()> {
        let (keycode, level_state) = self.resolve_keysym(sym).ok_or_else(|| {
            xerr(
                "unsupported",
                format!("keysym {sym:#x} is not in the display's keyboard map"),
            )
        })?;
        let o = self.key_origin(client)?;
        let mut held: Vec<(u8, u16)> = Vec::new();
        let mut state = 0u16;
        for &m in mods {
            let (mask, msym) = self.keymap.modifier(m);
            if let Some((kc, _)) = self.resolve_keysym(msym) {
                self.key_event(&o, true, kc, state, *time)?;
                held.push((kc, mask));
            }
            state |= mask;
        }
        // Explicit modifiers win; a level shift is added only when needed.
        let key_state = state | level_state;
        *time = time.wrapping_add(1);
        self.key_event(&o, true, keycode, key_state, *time)?;
        *time = time.wrapping_add(1);
        self.key_event(&o, false, keycode, key_state, *time)?;
        for (kc, mask) in held.into_iter().rev() {
            *time = time.wrapping_add(1);
            self.key_event(&o, false, kc, state, *time)?;
            state &= !mask;
        }
        Ok(())
    }

    fn serve_selection(&mut self, e: xproto::SelectionRequestEvent) -> Result<()> {
        let a = &self.atoms;
        let prop = if e.property == NONE {
            e.target
        } else {
            e.property
        };
        let mut reply_prop = NONE;
        if e.selection == a.CLIPBOARD && e.owner == self.helper {
            if let Some(clip) = self.clipboard.as_mut() {
                let max = self.conn.maximum_request_bytes().saturating_sub(64);
                if e.target == a.TARGETS {
                    let targets = [a.TARGETS, a.UTF8_STRING, AtomEnum::STRING.into(), a.TEXT];
                    self.conn.change_property32(
                        PropMode::REPLACE,
                        e.requestor,
                        prop,
                        AtomEnum::ATOM,
                        &targets,
                    )?;
                    reply_prop = prop;
                } else if (e.target == a.UTF8_STRING || e.target == a.TEXT)
                    && clip.text.len() <= max
                {
                    self.conn.change_property8(
                        PropMode::REPLACE,
                        e.requestor,
                        prop,
                        a.UTF8_STRING,
                        clip.text.as_bytes(),
                    )?;
                    clip.served += 1;
                    reply_prop = prop;
                } else if e.target == u32::from(AtomEnum::STRING) && clip.text.len() <= max {
                    let latin1: Vec<u8> = clip
                        .text
                        .chars()
                        .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
                        .collect();
                    self.conn.change_property8(
                        PropMode::REPLACE,
                        e.requestor,
                        prop,
                        AtomEnum::STRING,
                        &latin1,
                    )?;
                    clip.served += 1;
                    reply_prop = prop;
                }
            }
        }
        let notify = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: reply_prop,
        };
        self.conn
            .send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Window-bound input. Supported methods: click, drag, scroll,
    /// type_text, press_key, paste. Semantic methods (set_value,
    /// perform_secondary_action) and element_index targeting belong to the
    /// accessibility layer and are rejected here.
    pub fn action(&mut self, id: u32, method: &str, p: &Value) -> Result<()> {
        let allowed: &[&str] = match method {
            "click" => &["window_id", "x", "y", "button", "click_count"],
            "drag" => &["window_id", "from", "to"],
            "scroll" => &["window_id", "x", "y", "direction", "pixels"],
            "type_text" => &["window_id", "text"],
            "press_key" => &["window_id", "key"],
            "paste" => &["window_id", "text"],
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
            if w.as_u64() != Some(id as u64) {
                return Err(invalid("window_id parameter does not match target window"));
            }
        }
        self.ensure_no_abort()?;
        let info = self.window(id)?;
        self.ensure_damage(id);
        self.settle_target = Some(id);
        let mut time = self.server_time()?;
        let res = match method {
            "click" => self.do_click(&info, p, &mut time),
            "drag" => self.do_drag(&info, p, &mut time),
            "scroll" => self.do_scroll(&info, p, &mut time),
            "type_text" => self.do_type(&info, p, &mut time),
            "press_key" => self.do_key(&info, p, &mut time),
            "paste" => self.do_paste(&info, p, &mut time),
            _ => unreachable!(),
        };
        self.conn.flush()?;
        res
    }

    fn do_click(&mut self, info: &WindowInfo, p: &Value, time: &mut u32) -> Result<()> {
        let (x, y) = point(p, "x", "y", info)?;
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
        let mask = 0x80u16 << button; // Button1Mask = 0x100
        let t = self.move_to(info.window_id, x, y, *time, 0)?;
        for _ in 0..count {
            *time = time.wrapping_add(10);
            self.send(
                t.win,
                self.pointer_event(xproto::BUTTON_PRESS_EVENT, button, *time, &t, x, y, 0),
            )?;
            *time = time.wrapping_add(10);
            self.send(
                t.win,
                self.pointer_event(xproto::BUTTON_RELEASE_EVENT, button, *time, &t, x, y, mask),
            )?;
        }
        Ok(())
    }

    fn do_drag(&mut self, info: &WindowInfo, p: &Value, time: &mut u32) -> Result<()> {
        let (x0, y0) = pair(p, "from", info)?;
        let (x1, y1) = pair(p, "to", info)?;
        const B1: u16 = 0x100;
        let t = self.move_to(info.window_id, x0, y0, *time, 0)?;
        *time = time.wrapping_add(10);
        self.send(
            t.win,
            self.pointer_event(xproto::BUTTON_PRESS_EVENT, 1, *time, &t, x0, y0, 0),
        )?;
        self.conn.flush()?;
        // Implicit grab semantics: motion and release go to the press window.
        let (mut cx, mut cy) = (x0, y0);
        let mut aborted = None;
        for i in 1..=DRAG_STEPS {
            cx = x0 + (x1 - x0) * i / DRAG_STEPS;
            cy = y0 + (y1 - y0) * i / DRAG_STEPS;
            *time = time.wrapping_add(16);
            self.send(
                t.win,
                self.pointer_event(xproto::MOTION_NOTIFY_EVENT, 0, *time, &t, cx, cy, B1),
            )?;
            self.conn.flush()?;
            if let Err(e) = self.ensure_no_abort() {
                aborted = Some(e);
                break;
            }
        }
        *time = time.wrapping_add(10);
        self.send(
            t.win,
            self.pointer_event(xproto::BUTTON_RELEASE_EVENT, 1, *time, &t, cx, cy, B1),
        )?;
        self.cursors.insert(info.window_id, (cx, cy));
        match aborted {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn do_scroll(&mut self, info: &WindowInfo, p: &Value, time: &mut u32) -> Result<()> {
        let (x, y) = point(p, "x", "y", info)?;
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
        let t = self.move_to(info.window_id, x, y, *time, 0)?;
        let release_mask = if button <= 5 { 0x80u16 << button } else { 0 };
        for _ in 0..notches {
            *time = time.wrapping_add(5);
            self.send(
                t.win,
                self.pointer_event(xproto::BUTTON_PRESS_EVENT, button, *time, &t, x, y, 0),
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
        Ok(())
    }

    fn do_type(&mut self, info: &WindowInfo, p: &Value, time: &mut u32) -> Result<()> {
        let text = text_param(p)?;
        // Resolve everything first: never send partial text for unmappable input.
        let mut plan = Vec::with_capacity(text.len());
        let mut missing = Vec::new();
        let mut prev_cr = false;
        for c in text.chars() {
            if c == '\n' && prev_cr {
                prev_cr = false;
                continue;
            }
            prev_cr = c == '\r';
            let sym = char_keysym(c);
            let hit = sym
                .and_then(|s| self.resolve_keysym(s))
                .or_else(|| legacy_keysym(c).and_then(|s| self.resolve_keysym(s)));
            match hit {
                Some(k) => plan.push(k),
                None => {
                    if !missing.contains(&c) {
                        missing.push(c)
                    }
                }
            }
        }
        if !missing.is_empty() {
            let list: String = missing
                .iter()
                .take(20)
                .map(|c| format!("{:?} ", c))
                .collect();
            return Err(xerr(
                "unsupported",
                format!(
                    "characters not in the display keyboard map (use paste instead): {}",
                    list.trim_end()
                ),
            ));
        }
        let o = self.key_origin(info.window_id)?;
        let shift_kc = self.resolve_keysym(XK_SHIFT_L).map(|k| k.0);
        for (i, (kc, state)) in plan.into_iter().enumerate() {
            let shift = shift_kc.filter(|_| state & SHIFT != 0);
            if let Some(skc) = shift {
                self.key_event(&o, true, skc, 0, *time)?;
            }
            *time = time.wrapping_add(1);
            self.key_event(&o, true, kc, state, *time)?;
            *time = time.wrapping_add(1);
            self.key_event(&o, false, kc, state, *time)?;
            if let Some(skc) = shift {
                self.key_event(&o, false, skc, SHIFT, *time)?;
            }
            if i % 32 == 31 {
                self.conn.flush()?;
                self.ensure_no_abort()?;
            }
        }
        Ok(())
    }

    fn do_key(&mut self, info: &WindowInfo, p: &Value, time: &mut u32) -> Result<()> {
        let key = p
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("key must be a string such as \"Return\" or \"ctrl+s\""))?;
        let (mods, sym) = parse_key_combo(key)?;
        if self.resolve_keysym(sym).is_none() {
            return Err(xerr(
                "unsupported",
                format!("key {key:?} is not in the display keyboard map"),
            ));
        }
        self.send_combo(info.window_id, &mods, sym, time)
    }

    fn do_paste(&mut self, info: &WindowInfo, p: &Value, time: &mut u32) -> Result<()> {
        let text = text_param(p)?;
        let max = self.conn.maximum_request_bytes().saturating_sub(64);
        if text.len() > max {
            return Err(invalid(format!(
                "paste text exceeds {max} bytes (INCR transfers unsupported)"
            )));
        }
        let clip_atom = self.atoms.CLIPBOARD;
        self.conn
            .set_selection_owner(self.helper, clip_atom, *time)?;
        if self.conn.get_selection_owner(clip_atom)?.reply()?.owner != self.helper {
            return Err(xerr(
                "input_failed",
                "could not acquire CLIPBOARD ownership",
            ));
        }
        self.clipboard = Some(Clipboard {
            text: text.to_string(),
            expires: Instant::now() + CLIPBOARD_LIFETIME,
            served: 0,
        });
        // Conversions that happened before the keystroke do not count.
        self.pump()?;
        if let Some(c) = self.clipboard.as_mut() {
            c.served = 0;
        }
        self.send_combo(info.window_id, &[Modifier::Ctrl], 'v' as u32, time)?;
        self.conn.flush()?;
        let deadline = Instant::now() + PASTE_CONSUME_TIMEOUT;
        loop {
            self.ensure_no_abort()?;
            match &self.clipboard {
                Some(c) if c.served > 0 => return Ok(()),
                Some(_) => {}
                None => {
                    return Err(xerr(
                        "input_failed",
                        "clipboard ownership lost before paste completed",
                    ))
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(xerr(
                    "input_failed",
                    "target did not request the clipboard after synthetic Ctrl+V; it may ignore synthetic key events",
                ));
            }
            self.wait_readable(deadline - now)?;
        }
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

fn check_in_window(x: i64, y: i64, info: &WindowInfo) -> Result<(i32, i32)> {
    if x < 0 || y < 0 || x >= info.width as i64 || y >= info.height as i64 {
        return Err(invalid(format!(
            "point ({x}, {y}) is outside window {} ({}x{})",
            info.window_id, info.width, info.height
        )));
    }
    Ok((x as i32, y as i32))
}

fn point(p: &Value, kx: &str, ky: &str, info: &WindowInfo) -> Result<(i32, i32)> {
    let x = p
        .get(kx)
        .and_then(int)
        .ok_or_else(|| invalid(format!("{kx} must be an integer")))?;
    let y = p
        .get(ky)
        .and_then(int)
        .ok_or_else(|| invalid(format!("{ky} must be an integer")))?;
    check_in_window(x, y, info)
}

fn pair(p: &Value, key: &str, info: &WindowInfo) -> Result<(i32, i32)> {
    let arr = p
        .get(key)
        .and_then(Value::as_array)
        .filter(|a| a.len() == 2)
        .ok_or_else(|| invalid(format!("{key} must be [x, y]")))?;
    match (int(&arr[0]), int(&arr[1])) {
        (Some(x), Some(y)) => check_in_window(x, y, info),
        _ => Err(invalid(format!("{key} must contain integers"))),
    }
}

fn text_param(p: &Value) -> Result<&str> {
    let t = p
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("text must be a string"))?;
    if t.is_empty() {
        return Err(invalid("text must not be empty"));
    }
    if t.chars().count() > MAX_TEXT_CHARS {
        return Err(invalid(format!(
            "text longer than {MAX_TEXT_CHARS} characters"
        )));
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        assert_eq!(
            point(&json!({"x": 10, "y": 49}), "x", "y", &info).unwrap(),
            (10, 49)
        );
        assert!(point(&json!({"x": 100, "y": 0}), "x", "y", &info).is_err());
        assert!(point(&json!({"x": -1, "y": 0}), "x", "y", &info).is_err());
        assert!(point(&json!({"x": 1.5, "y": 0}), "x", "y", &info).is_err());
        assert_eq!(
            pair(&json!({"from": [1.0, 2]}), "from", &info).unwrap(),
            (1, 2)
        );
        assert!(pair(&json!({"from": [1]}), "from", &info).is_err());
        assert!(text_param(&json!({"text": ""})).is_err());
        assert_eq!(char_keysym('é'), Some(0xe9));
        assert_eq!(char_keysym('ж'), Some(0x0100_0436));
        assert_eq!(char_keysym('\u{7}'), None);
    }
}
