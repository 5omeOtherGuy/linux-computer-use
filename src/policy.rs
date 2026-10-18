use crate::x11::WindowInfo;
use std::collections::HashSet;

/// Authorization outcome for one window.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    NeedsApproval,
    Denied(&'static str),
}

#[derive(Default)]
pub struct Policy {
    allow: HashSet<String>,
    deny: HashSet<String>,
    /// User-approved apps for this turn: (server-verified PID, process
    /// start time, app ID). An approval covers every window of that process
    /// and app (dialogs too); a new process that reuses the PID does not
    /// inherit it.
    approved: HashSet<(u32, u64, String)>,
}

impl Policy {
    pub fn from_env() -> Self {
        fn entries(key: &str) -> HashSet<String> {
            std::env::var(key)
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        }
        Self {
            allow: entries("CUA_ALLOW_APPS"),
            deny: entries("CUA_DENY_APPS"),
            approved: HashSet::new(),
        }
    }

    /// Denial reason, if any. `w.pid` must be the X-Resource-verified PID:
    /// a window whose process identity or executable cannot be established
    /// is denied, since the executable hard-deny could not be checked.
    pub fn denied(&self, w: &WindowInfo) -> Option<&'static str> {
        let app = w.app_id.to_lowercase();
        if app == "unknown" || app.is_empty() {
            return Some("the app has no identity (WM_CLASS)");
        }
        if self.deny.contains(&app) || forbidden(&app) {
            return Some("the app is denied by policy");
        }
        if w.pid == 0 {
            return Some(
                "the X server cannot verify the window's process (remote or unknown client)",
            );
        }
        let Some(executable) = std::fs::read_link(format!("/proc/{}/exe", w.pid))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
        else {
            return Some("the window's executable cannot be determined");
        };
        if forbidden(&executable) {
            return Some("the app's executable is denied by policy");
        }
        None
    }

    pub fn decide(&self, w: &WindowInfo) -> Decision {
        if let Some(reason) = self.denied(w) {
            return Decision::Denied(reason);
        }
        if self.allow.contains(&w.app_id.to_lowercase())
            || start_time(w.pid)
                .is_some_and(|t| self.approved.contains(&(w.pid, t, w.app_id.clone())))
        {
            Decision::Allowed
        } else {
            Decision::NeedsApproval
        }
    }

    /// Denial reason for launching an application known by `names` (its
    /// desktop ID, menu name, program and WM class): the deny list and the
    /// hard denies apply to any of them.
    pub fn launch_denied(&self, names: &[String]) -> Option<&'static str> {
        for n in names {
            let n = n.trim().to_lowercase();
            if n.is_empty() {
                continue;
            }
            if forbidden(&n) {
                return Some("the application is denied by policy");
            }
            if self.deny.contains(&n) {
                return Some("the application is on the deny list");
            }
        }
        None
    }

    pub fn approve(&mut self, w: &WindowInfo) {
        if let Some(t) = start_time(w.pid) {
            self.approved.insert((w.pid, t, w.app_id.clone()));
        }
    }

    pub fn reset(&mut self) {
        self.approved.clear();
    }
}

/// Start time of a process (clock ticks since boot, /proc/<pid>/stat field
/// 22), which distinguishes a reused PID.
fn start_time(pid: u32) -> Option<u64> {
    parse_start_time(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

fn parse_start_time(stat: &str) -> Option<u64> {
    // The command name (field 2) may contain spaces and parentheses.
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    rest.split(' ').nth(19)?.parse().ok()
}

/// Executable path of a server-verified PID, for approval records that bind
/// an app ID to the program behind it.
pub fn executable(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

fn forbidden(s: &str) -> bool {
    // Denials take precedence even for explicitly allowed apps. Classification
    // is deliberately conservative; X11 client metadata is not an identity trust root.
    [
        "terminal",
        "xterm",
        "uxterm",
        "rxvt",
        "konsole",
        "kitty",
        "alacritty",
        "wezterm",
        "ghostty",
        "terminator",
        "tilix",
        "guake",
        "yakuake",
        "sakura",
        "termite",
        "cool-retro-term",
        "tilda",
        "qterminal",
        "lxterminal",
        "hyper",
        "keepass",
        "bitwarden",
        "1password",
        "onepassword",
        "enpass",
        "dashlane",
        "lastpass",
        "seahorse",
        "kwallet",
        "gcr-prompter",
        "polkit",
        "pinentry",
    ]
    .iter()
    .any(|name| s.contains(name))
        || matches!(s, "st" | "foot" | "pass" | "gopass")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn window(app: &str) -> WindowInfo {
        WindowInfo {
            window_id: 15,
            // A real, readable process whose executable is not denied.
            pid: std::process::id(),
            app_id: app.into(),
            title: "Test".into(),
            width: 300,
            height: 200,
            transient_for: None,
            origin: (0, 0),
        }
    }
    fn allowed(p: &Policy, w: &WindowInfo) -> bool {
        p.decide(w) == Decision::Allowed
    }
    #[test]
    fn deny_overrides_approval_and_explicit_allow() {
        for name in ["org.gnome.Terminal", "KeePassXC", "kitty", "Bitwarden"] {
            let w = window(name);
            let mut p = Policy::default();
            p.allow.insert(name.to_lowercase());
            p.approve(&w);
            assert!(p.denied(&w).is_some());
            assert!(!allowed(&p, &w));
        }
        let w = window("Editor");
        let mut p = Policy::default();
        p.approve(&w);
        assert!(allowed(&p, &w));
        p.deny.insert("editor".into());
        assert!(!allowed(&p, &w));
    }
    #[test]
    fn approvals_bind_identity_and_end_with_turn() {
        let w = window("Fixture");
        let mut p = Policy::default();
        assert_eq!(p.decide(&w), Decision::NeedsApproval);
        p.approve(&w);
        assert!(allowed(&p, &w));
        let mut replacement = w.clone();
        replacement.pid = 1;
        assert!(!allowed(&p, &replacement));
        // A dialog of the same process and app is covered by the approval.
        let mut dialog = w.clone();
        dialog.window_id = 16;
        dialog.transient_for = Some(15);
        assert!(allowed(&p, &dialog));
        p.reset();
        assert!(!allowed(&p, &w));
    }
    /// Regression: approval was keyed by (PID, app ID) only, so a new
    /// process reusing an approved PID with the same WM_CLASS inherited it.
    #[test]
    fn approval_is_bound_to_the_process_start_time() {
        let w = window("Fixture");
        let mut p = Policy::default();
        // Approved for an earlier process that had this PID.
        let now = start_time(w.pid).unwrap();
        p.approved.insert((w.pid, now + 1, w.app_id.clone()));
        assert!(!allowed(&p, &w));
        p.approve(&w);
        assert!(allowed(&p, &w));
        let stat = "123 (a b) c) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242 99";
        assert_eq!(parse_start_time(stat), Some(4242));
        assert_eq!(parse_start_time("garbage"), None);
    }

    #[test]
    fn launching_follows_the_deny_lists() {
        let mut p = Policy::default();
        p.deny.insert("drawing".into());
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(p
            .launch_denied(&names(&[
                "com.github.maoschanz.drawing",
                "Drawing",
                "drawing"
            ]))
            .is_some());
        assert!(p
            .launch_denied(&names(&["org.gnome.Terminal", "Terminal"]))
            .is_some());
        assert!(p
            .launch_denied(&names(&["xed", "Text Editor", "xed"]))
            .is_none());
    }

    /// Regression: an unknown process identity used to skip the executable
    /// hard-deny (fail open) and let an allow-listed app through.
    #[test]
    fn unknown_process_identity_is_denied_even_if_allowed() {
        let mut p = Policy::default();
        p.allow.insert("fixture".into());
        let mut unverified = window("Fixture");
        unverified.pid = 0;
        assert!(matches!(p.decide(&unverified), Decision::Denied(_)));
        let mut vanished = window("Fixture");
        vanished.pid = u32::MAX - 1; // above pid_max: no /proc entry
        assert!(matches!(p.decide(&vanished), Decision::Denied(_)));
        assert_eq!(p.decide(&window("Fixture")), Decision::Allowed);
    }
}
