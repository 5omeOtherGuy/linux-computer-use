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
    approved: HashSet<(u32, u32, String)>,
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
            || self
                .approved
                .contains(&(w.window_id, w.pid, w.app_id.clone()))
        {
            Decision::Allowed
        } else {
            Decision::NeedsApproval
        }
    }

    pub fn approve(&mut self, w: &WindowInfo) {
        self.approved.insert((w.window_id, w.pid, w.app_id.clone()));
    }

    pub fn reset(&mut self) {
        self.approved.clear();
    }
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
        p.reset();
        assert!(!allowed(&p, &w));
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
