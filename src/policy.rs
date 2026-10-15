use crate::x11::WindowInfo;
use std::collections::HashSet;

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

    pub fn denied(&self, w: &WindowInfo) -> bool {
        let app = w.app_id.to_lowercase();
        let executable = std::fs::read_link(format!("/proc/{}/exe", w.pid))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        app == "unknown"
            || app.is_empty()
            || self.deny.contains(&app)
            || forbidden(&app)
            || forbidden(&executable)
    }

    pub fn allowed(&self, w: &WindowInfo) -> bool {
        !self.denied(w)
            && (self.allow.contains(&w.app_id.to_lowercase())
                || self
                    .approved
                    .contains(&(w.window_id, w.pid, w.app_id.clone())))
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
            pid: 0,
            app_id: app.into(),
            title: "Test".into(),
            width: 300,
            height: 200,
        }
    }
    #[test]
    fn deny_overrides_approval_and_explicit_allow() {
        for name in ["org.gnome.Terminal", "KeePassXC", "kitty", "Bitwarden"] {
            let w = window(name);
            let mut p = Policy::default();
            p.allow.insert(name.to_lowercase());
            p.approve(&w);
            assert!(p.denied(&w));
            assert!(!p.allowed(&w));
        }
        let w = window("Editor");
        let mut p = Policy::default();
        p.approve(&w);
        assert!(p.allowed(&w));
        p.deny.insert("editor".into());
        assert!(!p.allowed(&w));
    }
    #[test]
    fn approvals_bind_identity_and_end_with_turn() {
        let w = window("Fixture");
        let mut p = Policy::default();
        assert!(!p.allowed(&w));
        p.approve(&w);
        assert!(p.allowed(&w));
        let mut replacement = w.clone();
        replacement.pid = 99;
        assert!(!p.allowed(&replacement));
        p.reset();
        assert!(!p.allowed(&w));
    }
}
