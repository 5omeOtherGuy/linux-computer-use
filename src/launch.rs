//! Installed desktop applications (freedesktop desktop entries) for
//! `launch_app`: the model names an application the way a user would pick
//! it from the menu, never a command line. Only `Type=Application` entries
//! that a menu would show (not `NoDisplay`, not `Hidden`) are launchable,
//! and `Exec` field codes are expanded with no files or URLs.
//!
//! The apps the model drives run as the same user as the daemon, so a
//! desktop entry or program that this user can write is not trusted: it is
//! never launched, though an entry still hides the same ID in directories
//! of lower precedence.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Desktop file ID without the suffix (`org.gnome.Calculator`).
    pub id: String,
    /// `Name=` (untranslated).
    pub name: String,
    /// Command line: program and arguments.
    pub exec: Vec<String>,
    /// `Path=`: working directory.
    pub path: Option<String>,
    /// `StartupWMClass=`.
    pub wm_class: Option<String>,
    /// `Terminal=true`: runs in a terminal emulator (never launched).
    pub terminal: bool,
}

impl Entry {
    /// Names an app can be matched or denied by: ID, its last segment,
    /// menu name, program basename and WM class.
    pub fn names(&self) -> Vec<String> {
        let mut out = vec![self.id.clone(), self.name.clone()];
        if let Some(tail) = self.id.rsplit('.').next() {
            out.push(tail.to_string());
        }
        if let Some(prog) = self.exec.first().and_then(|p| Path::new(p).file_name()) {
            out.push(prog.to_string_lossy().into_owned());
        }
        if let Some(c) = &self.wm_class {
            out.push(c.clone());
        }
        out
    }
}

/// Where desktop entries live, in precedence order (`XDG_DATA_HOME`, then
/// `XDG_DATA_DIRS`).
pub fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".local/share")));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    data_home
        .into_iter()
        .chain(
            data_dirs
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        )
        .map(|d| d.join("applications"))
        .collect()
}

/// All launchable entries in `dirs`. A file's desktop ID is its path below
/// the directory with `/` replaced by `-` (`kde/foo.desktop` is `kde-foo`).
/// The first file with an ID decides it (freedesktop precedence): one that
/// is `Hidden`, `NoDisplay`, not an application, unreadable or not
/// `trusted` hides the same ID in later directories instead of falling
/// through to them. Fails when a directory holds more than `MAX_FILES`
/// entries, rather than skip what it could not read.
pub fn load(
    dirs: &[PathBuf],
    trusted: &dyn Fn(&Path) -> bool,
) -> std::result::Result<Vec<Entry>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Entry> = Vec::new();
    for dir in dirs {
        let mut files = Vec::new();
        let mut budget = MAX_FILES;
        if !desktop_files(dir, "", 0, &mut budget, &mut files) {
            return Err(format!(
                "{} holds more than {MAX_FILES} files; no application is launched",
                dir.display()
            ));
        }
        files.sort();
        for (file, id) in files {
            if !seen.insert(id.clone()) || !trusted(&file) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            if let Some(e) = parse(&id, &text) {
                out.push(e);
            }
        }
    }
    Ok(out)
}

const MAX_FILES: usize = 10_000;

/// Collects `(path, desktop ID)` of the `.desktop` files in `dir` and its
/// subdirectories, at most 8 levels deep and never through a symlinked
/// directory (so there are no cycles). False when more than `budget`
/// directory entries were met.
fn desktop_files(
    dir: &Path,
    prefix: &str,
    depth: u32,
    budget: &mut usize,
    out: &mut Vec<(PathBuf, String)>,
) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return true;
    };
    for e in rd.flatten() {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        let (path, name) = (e.path(), e.file_name());
        let Some(name) = name.to_str() else { continue };
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            if depth < 8
                && !desktop_files(&path, &format!("{prefix}{name}-"), depth + 1, budget, out)
            {
                return false;
            }
        } else if let Some(stem) = name.strip_suffix(".desktop") {
            out.push((path, format!("{prefix}{stem}")));
        }
    }
    true
}

/// Whether this user cannot change what the absolute `path` names: every
/// directory and file met while resolving it, through every symlink, is
/// owned by another user (normally root) and writable by neither group nor
/// others. Fails closed: a relative path, one that cannot be resolved or
/// examined, and everything while the daemon runs as root are not trusted.
pub fn trusted(path: &Path) -> bool {
    let euid = unsafe { libc::geteuid() };
    let safe = |m: &std::fs::Metadata| m.uid() != euid && m.mode() & 0o022 == 0;
    let parts = |p: &Path| -> Vec<std::ffi::OsString> {
        p.components()
            .rev()
            .map(|c| c.as_os_str().to_owned())
            .collect()
    };
    if !path.is_absolute() || !std::fs::metadata("/").is_ok_and(|m| safe(&m)) {
        return false;
    }
    let mut cur = PathBuf::from("/");
    let mut todo = parts(path);
    let mut links = 0;
    while let Some(part) = todo.pop() {
        match part.to_str() {
            Some("/") => cur = PathBuf::from("/"),
            Some(".") => {}
            Some("..") => {
                cur.pop();
            }
            _ => {
                let next = cur.join(&part);
                let Ok(m) = std::fs::symlink_metadata(&next) else {
                    return false;
                };
                if m.file_type().is_symlink() {
                    // The link lives in `cur`, which is trusted; where it
                    // points is checked as the walk continues.
                    links += 1;
                    let Ok(target) = std::fs::read_link(&next) else {
                        return false;
                    };
                    if links > 40 {
                        return false;
                    }
                    todo.extend(parts(&target));
                } else if safe(&m) {
                    cur = next;
                } else {
                    return false;
                }
            }
        }
    }
    true
}

/// How an entry is started.
#[derive(Debug, PartialEq, Eq)]
pub struct Command {
    /// Absolute, trusted program; `argv[0]` stays `entry.exec[0]`.
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Trusted working directory: `Path=`, else `/`.
    pub cwd: PathBuf,
    /// The child's `PATH`: the trusted absolute directories of `path_var`.
    pub path: String,
}

/// The command for `entry`, with every input this user could have written
/// refused: the program, an `env` launcher's command and the interpreters
/// of a script (`#!` lines, `#!/usr/bin/env NAME` looked up in the child's
/// `PATH`) must resolve to `trusted` files, `Path=` must be a trusted
/// directory, and the child's `PATH` holds only trusted directories. What
/// the app itself runs later, its configuration and its plugins are outside
/// this check.
pub fn command(
    entry: &Entry,
    path_var: &str,
    trusted: &dyn Fn(&Path) -> bool,
) -> std::result::Result<Command, String> {
    let dirs: Vec<&str> = path_var
        .split(':')
        .filter(|d| Path::new(d).is_absolute() && trusted(Path::new(d)))
        .collect();
    if dirs.is_empty() {
        return Err("no PATH directory is outside this user's reach".into());
    }
    let path = dirs.join(":");
    let resolve = |prog: &str| {
        let p = resolve_program(prog, path_var, trusted).ok_or_else(|| {
            format!("{prog} is not an executable in a location this user cannot write")
        })?;
        interpreters(&p, &path, trusted, 0)?;
        Ok::<_, String>(p)
    };
    let program = resolve(&entry.exec[0])?;
    let mut args = entry.exec[1..].to_vec();
    if program.file_name().is_some_and(|n| n == "env") {
        // `env [NAME=VALUE ...] COMMAND [ARG ...]` only: its command is
        // resolved here, so `env` never searches a PATH on its own.
        let assignment = |a: &str| a.find('=').is_some_and(|i| i > 0) && !a.starts_with('-');
        let Some(i) = args.iter().position(|a| !assignment(a)) else {
            return Err("its env launcher names no command".into());
        };
        if args[i].starts_with('-') {
            return Err(format!("its env launcher uses an option ({})", args[i]));
        }
        if args[..i].iter().any(|a| a.starts_with("PATH=")) {
            return Err("its env launcher sets PATH".into());
        }
        let cmd = resolve(&args[i])?.to_string_lossy().into_owned();
        if cmd.contains('=') {
            return Err(format!("its env launcher's command {cmd} contains '='"));
        }
        args[i] = cmd;
    }
    let cwd = match &entry.path {
        None => PathBuf::from("/"),
        Some(p) if Path::new(p).is_absolute() && Path::new(p).is_dir() && trusted(Path::new(p)) => {
            PathBuf::from(p)
        }
        Some(p) => {
            return Err(format!(
                "its working directory {p} is missing, relative or writable by this user"
            ))
        }
    };
    Ok(Command {
        program,
        args,
        cwd,
        path,
    })
}

/// Checks the interpreter chain of `program` (a script's `#!` line, up to
/// five levels): each interpreter must be `trusted`, and for
/// `#!/usr/bin/env NAME` every `NAME` in `path` must be, since `env` moves
/// on to the next one when one fails to execute. The line is read as the
/// kernel reads it (bytes, split at spaces and tabs); anything else in it,
/// such as a carriage return, invalid UTF-8 or env options, is refused.
fn interpreters(
    program: &Path,
    path: &str,
    trusted: &dyn Fn(&Path) -> bool,
    depth: u32,
) -> std::result::Result<(), String> {
    let refuse = |why: &str| Err(format!("{}: {why}", program.display()));
    if depth > 5 {
        return refuse("interpreters nest too deeply");
    }
    let mut head = [0u8; 256];
    let n = match std::fs::File::open(program)
        .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
    {
        Ok(n) => n,
        Err(e) => return refuse(&format!("cannot read it ({e})")),
    };
    let Some(rest) = head[..n].strip_prefix(b"#!") else {
        return Ok(());
    };
    let Some(end) = rest.iter().position(|&b| b == b'\n') else {
        return refuse("its #! line is too long");
    };
    let Ok(line) = std::str::from_utf8(&rest[..end]) else {
        return refuse("its #! line is not UTF-8");
    };
    if line.chars().any(|c| c.is_control() && c != '\t') {
        return refuse("its #! line has control characters");
    }
    let line = line.trim_matches([' ', '\t']);
    let (interp, arg) = line.split_once([' ', '\t']).unwrap_or((line, ""));
    let (interp, arg) = (Path::new(interp), arg.trim_matches([' ', '\t']));
    if !interp.is_absolute() || !trusted(interp) {
        return refuse(&format!(
            "its interpreter {} is not trusted",
            interp.display()
        ));
    }
    if interp.file_name().is_none_or(|n| n != "env") {
        return interpreters(interp, path, trusted, depth + 1);
    }
    if arg.is_empty() || arg.starts_with('-') || arg.contains(['=', '/', ' ', '\t']) {
        return refuse(&format!("unsupported #! line {line}"));
    }
    let mut found = false;
    for candidate in path.split(':').map(|d| Path::new(d).join(arg)) {
        if std::fs::symlink_metadata(&candidate).is_err() {
            continue;
        }
        if !trusted(&candidate) {
            return refuse(&format!("{} is not trusted", candidate.display()));
        }
        if candidate.is_file() {
            interpreters(&candidate, path, trusted, depth + 1)?;
            found = true;
        }
    }
    if !found {
        return refuse(&format!("{arg} is not on PATH"));
    }
    Ok(())
}

/// The absolute path of the program an entry runs: `program` itself when it
/// is absolute, else the first executable file of that name in a `PATH`
/// directory. Only `trusted` locations count, so a program this user could
/// have written (`~/.local/bin`, a writable `PATH` entry) is never started.
pub fn resolve_program(
    program: &str,
    path_var: &str,
    trusted: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let runnable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0) && trusted(p)
    };
    if program.contains('/') {
        let p = Path::new(program);
        return (p.is_absolute() && runnable(p)).then(|| p.to_path_buf());
    }
    path_var
        .split(':')
        .map(Path::new)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(program))
        .find(|p| runnable(p))
}

/// The `[Desktop Entry]` group of a desktop file, if it is a launchable
/// application.
pub fn parse(id: &str, text: &str) -> Option<Entry> {
    let mut in_group = false;
    let mut kind = None;
    let mut name = None;
    let mut exec = None;
    let mut path = None;
    let mut wm_class = None;
    let mut terminal = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(group) = line.strip_prefix('[') {
            in_group = group.trim_end_matches(']') == "Desktop Entry";
            continue;
        }
        if !in_group {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "Type" => kind = Some(v.to_string()),
            "Name" => name = Some(v.to_string()),
            "Exec" => exec = Some(v.to_string()),
            "Path" => path = Some(v.to_string()).filter(|p| !p.is_empty()),
            "StartupWMClass" => wm_class = Some(v.to_string()).filter(|c| !c.is_empty()),
            "Terminal" => terminal = v.eq_ignore_ascii_case("true"),
            "NoDisplay" | "Hidden" if v.eq_ignore_ascii_case("true") => return None,
            _ => {}
        }
    }
    if kind.as_deref() != Some("Application") {
        return None;
    }
    let name = name?;
    let exec = exec_args(&exec?, &name).ok()?;
    Some(Entry {
        id: id.to_string(),
        name,
        exec,
        path,
        wm_class,
        terminal,
    })
}

/// Splits an `Exec=` value into arguments (desktop entry quoting) and
/// expands its field codes: `%c` is the name, `%%` a percent sign, and
/// the file, URL, icon and location codes expand to nothing.
pub fn exec_args(exec: &str, name: &str) -> Result<Vec<String>> {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' => quoted = false,
                '\\' => cur.push(
                    chars
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("dangling backslash"))?,
                ),
                _ => cur.push(c),
            }
        } else {
            match c {
                '"' => {
                    quoted = true;
                    started = true;
                }
                ' ' | '\t' => {
                    if started || !cur.is_empty() {
                        words.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                _ => cur.push(c),
            }
        }
    }
    if quoted {
        bail!("unterminated quote in Exec");
    }
    if started || !cur.is_empty() {
        words.push(cur);
    }
    let mut out = Vec::new();
    for word in words {
        let mut arg = String::new();
        let mut drop_if_empty = false;
        let mut it = word.chars();
        while let Some(c) = it.next() {
            if c != '%' {
                arg.push(c);
                continue;
            }
            match it.next() {
                Some('%') => arg.push('%'),
                Some('c') => arg.push_str(name),
                Some('f' | 'F' | 'u' | 'U' | 'd' | 'D' | 'n' | 'N' | 'i' | 'k' | 'v' | 'm') => {
                    drop_if_empty = true;
                }
                Some(o) => bail!("unknown field code %{o} in Exec"),
                None => bail!("dangling % in Exec"),
            }
        }
        if !(drop_if_empty && arg.is_empty()) {
            out.push(arg);
        }
    }
    if out.is_empty() {
        bail!("Exec names no program");
    }
    Ok(out)
}

pub enum Lookup {
    Found(Entry),
    /// Several entries match; their IDs.
    Ambiguous(Vec<String>),
    NotFound,
}

/// The entry a name picks: an exact (case-insensitive) match on an ID, its
/// last segment, the menu name, the program or the WM class; failing that,
/// the only entry whose ID or name contains the text.
pub fn find(entries: &[Entry], query: &str) -> Lookup {
    let q = query.trim().to_lowercase();
    let q = q.strip_suffix(".desktop").unwrap_or(&q).to_string();
    if q.is_empty() {
        return Lookup::NotFound;
    }
    let exact: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.names().iter().any(|n| n.to_lowercase() == q))
        .collect();
    let pick = |hits: Vec<&Entry>| match hits.as_slice() {
        [] => None,
        [one] => Some(Lookup::Found((*one).clone())),
        many => Some(Lookup::Ambiguous(
            many.iter().map(|e| e.id.clone()).take(8).collect(),
        )),
    };
    if let Some(r) = pick(exact) {
        return r;
    }
    let partial: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.id.to_lowercase().contains(&q) || e.name.to_lowercase().contains(&q))
        .collect();
    pick(partial).unwrap_or(Lookup::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting_and_field_codes() {
        assert_eq!(exec_args("drawing %U", "Drawing").unwrap(), vec!["drawing"]);
        assert_eq!(
            exec_args("env FOO=1 \"my app\" --name=%c %f --x", "My App").unwrap(),
            vec!["env", "FOO=1", "my app", "--name=My App", "--x"]
        );
        assert_eq!(
            exec_args("a \"b \\\"c\\\" d\" 100%%", "n").unwrap(),
            vec!["a", "b \"c\" d", "100%"]
        );
        assert!(exec_args("\"unterminated", "n").is_err());
        assert!(exec_args("prog %z", "n").is_err());
        assert!(exec_args("%F", "n").is_err());
    }

    #[test]
    fn parse_and_find() {
        let calc = parse(
            "org.gnome.Calculator",
            "[Desktop Entry]\nName=Calculator\nName[de]=Rechner\nType=Application\nExec=gnome-calculator\nTerminal=false\n[Desktop Action x]\nExec=other\n",
        )
        .unwrap();
        assert_eq!(calc.exec, vec!["gnome-calculator"]);
        assert!(parse(
            "hidden",
            "[Desktop Entry]\nType=Application\nName=H\nExec=h\nNoDisplay=true\n"
        )
        .is_none());
        assert!(parse("link", "[Desktop Entry]\nType=Link\nName=L\nURL=x\n").is_none());
        let term = parse(
            "t",
            "[Desktop Entry]\nType=Application\nName=Term\nExec=xterm\nTerminal=true\n",
        )
        .unwrap();
        assert!(term.terminal);
        let drawing = parse(
            "com.github.maoschanz.drawing",
            "[Desktop Entry]\nName=Drawing\nType=Application\nExec=drawing %U\nStartupWMClass=drawing\n",
        )
        .unwrap();
        let xed = parse(
            "xed",
            "[Desktop Entry]\nName=Text Editor\nType=Application\nExec=xed %U\n",
        )
        .unwrap();
        let entries = vec![calc.clone(), drawing.clone(), xed.clone()];
        for q in [
            "drawing",
            "Drawing",
            "com.github.maoschanz.drawing.desktop",
            "DRAWING",
        ] {
            assert!(
                matches!(find(&entries, q), Lookup::Found(ref e) if e == &drawing),
                "{q}"
            );
        }
        assert!(matches!(find(&entries, "gnome-calculator"), Lookup::Found(ref e) if e == &calc));
        assert!(matches!(find(&entries, "calculator"), Lookup::Found(ref e) if e == &calc));
        assert!(matches!(find(&entries, "text editor"), Lookup::Found(ref e) if e == &xed));
        assert!(matches!(find(&entries, "Calc"), Lookup::Found(ref e) if e == &calc));
        assert!(matches!(find(&entries, "nothing"), Lookup::NotFound));
        assert!(matches!(find(&entries, ""), Lookup::NotFound));
        let twin = Entry {
            id: "other.drawing".into(),
            ..drawing.clone()
        };
        let more = vec![drawing.clone(), twin];
        assert!(matches!(find(&more, "drawing"), Lookup::Ambiguous(ref ids) if ids.len() == 2));
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("linux-cua-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Regression: a `Hidden`/`NoDisplay` entry did not hide the same ID in a
    /// lower-precedence directory, and entries this user can write were
    /// launched like system ones.
    #[test]
    fn first_entry_decides_and_untrusted_entries_are_not_launched() {
        let root = scratch("load");
        let (user, system) = (root.join("user"), root.join("system"));
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        let app = |name: &str, extra: &str| {
            format!("[Desktop Entry]\nType=Application\nName={name}\nExec={name}\n{extra}")
        };
        std::fs::write(user.join("xed.desktop"), app("xed", "Hidden=true\n")).unwrap();
        std::fs::write(user.join("calc.desktop"), app("calc", "NoDisplay=true\n")).unwrap();
        std::fs::write(user.join("drawing.desktop"), app("sh", "")).unwrap();
        std::fs::write(user.join("mine.desktop"), app("mine", "")).unwrap();
        std::fs::create_dir_all(user.join("kde")).unwrap();
        std::fs::write(
            user.join("kde/okular.desktop"),
            app("okular", "Hidden=true\n"),
        )
        .unwrap();
        // A symlinked directory is not followed, so a cycle costs nothing.
        std::os::unix::fs::symlink(".", user.join("loop")).unwrap();
        for id in ["xed", "calc", "drawing", "other", "kde-okular"] {
            std::fs::write(system.join(format!("{id}.desktop")), app(id, "")).unwrap();
        }
        let trusted = |p: &Path| !p.starts_with(&user);
        let ids: Vec<String> = load(&[user.clone(), system.clone()], &trusted)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, vec!["other"]);
        // The same files when everything is trusted.
        let ids: Vec<String> = load(&[user, system], &|_| true)
            .unwrap()
            .into_iter()
            .map(|e| e.exec[0].clone())
            .collect();
        assert_eq!(ids, vec!["sh", "mine", "other"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn commands_take_no_input_this_user_can_write() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("command");
        let (writable, system) = (root.join("home-bin"), root.join("usr-bin"));
        let put = |path: PathBuf, text: String| {
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        for d in [&writable, &system] {
            std::fs::create_dir_all(d).unwrap();
            for prog in ["env", "drawing", "python3"] {
                put(d.join(prog), "\x7fELF".into()); // not a script
            }
        }
        let (env, sys) = (system.join("env"), system.display());
        // Scripts: a trusted interpreter, an untrusted one, and env lookups.
        put(system.join("tool"), format!("#!{sys}/python3 -u\n"));
        put(
            system.join("bad"),
            format!("#!{}/python3\n", writable.display()),
        );
        put(
            system.join("envtool"),
            format!("#! {} python3\n", env.display()),
        );
        put(
            system.join("envopt"),
            format!("#!{} -S python3 -u\n", env.display()),
        );
        let trusted = |p: &Path| p.starts_with(&system);
        let path_var = format!("{}:{}", writable.display(), sys);
        let entry = |exec: &str, path: Option<&Path>| Entry {
            id: "x".into(),
            name: "X".into(),
            exec: exec_args(exec, "X").unwrap(),
            path: path.map(|p| p.display().to_string()),
            wm_class: None,
            terminal: false,
        };
        let run = |exec: &str, path_var: &str| command(&entry(exec, None), path_var, &trusted);
        let c = run("env FOO=1 drawing --new", &path_var).unwrap();
        assert_eq!(c.program, env);
        let drawing = system.join("drawing").display().to_string();
        assert_eq!(c.args, vec!["FOO=1".to_string(), drawing, "--new".into()]);
        assert_eq!(c.cwd, PathBuf::from("/"));
        assert_eq!(c.path, sys.to_string());
        for good in ["tool", "envtool", "env A=1 envtool"] {
            assert!(run(good, &path_var).is_ok(), "{good}");
        }
        // env finds the writable python3 first in this PATH.
        let writable_first = format!("{}:{}", writable.display(), sys);
        let all_trusted_dirs = |p: &Path| p.starts_with(&system) || p == writable;
        assert!(command(&entry("envtool", None), &writable_first, &all_trusted_dirs).is_err());
        // env moves on when the first python3 fails to execute, so every
        // python3 on PATH counts, not only the first trusted executable one.
        let both = root.join("both");
        std::fs::create_dir_all(&both).unwrap();
        put(both.join("python3"), "\x7fELF".into());
        let two_dirs = format!("{}:{}", both.display(), writable.display());
        let dirs_trusted = |p: &Path| {
            p.starts_with(&system) || p == both || p == writable || p == both.join("python3")
        };
        let envtool = format!("{sys}/envtool");
        assert!(command(&entry(&envtool, None), &two_dirs, &dirs_trusted).is_err());
        let one_dir = both.display().to_string();
        assert!(command(&entry(&envtool, None), &one_dir, &dirs_trusted).is_ok());
        // The kernel keeps a carriage return in the interpreter name.
        put(
            system.join("crlf"),
            format!("#!{} python3\r\n", env.display()),
        );
        put(system.join("crlf2"), format!("#!{sys}/python3\r\n"));
        for bad in [
            "crlf",
            "crlf2",
            "env -S drawing",
            "env -u=X drawing",
            "env FOO=1",
            "env missing",
            "env PATH=/home/u/bin drawing",
            "bad",
            "envopt",
        ] {
            assert!(run(bad, &path_var).is_err(), "{bad}");
        }
        assert!(run("drawing", "").is_err(), "no trusted PATH directory");
        assert!(run("drawing", &writable.display().to_string()).is_err());
        assert!(command(&entry("drawing", Some(&writable)), &path_var, &trusted).is_err());
        assert!(command(
            &entry("drawing", Some(Path::new("rel"))),
            &path_var,
            &|_| true
        )
        .is_err());
        let c = command(&entry("drawing", Some(&system)), &path_var, &trusted).unwrap();
        assert_eq!((c.program, c.cwd), (system.join("drawing"), system.clone()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn files_this_user_can_write_are_not_trusted() {
        let root = scratch("trust");
        let file = root.join("x.desktop");
        std::fs::write(&file, "").unwrap();
        assert!(!trusted(&file));
        assert!(!trusted(&root));
        assert!(!trusted(Path::new("relative/x.desktop")));
        assert!(!trusted(&root.join("missing")));
        // A link this user made to a system file is not trusted either.
        let link = root.join("link");
        std::os::unix::fs::symlink("/", &link).unwrap();
        assert!(!trusted(&link));
        if unsafe { libc::geteuid() } != 0 {
            let root_dir = std::fs::metadata("/").unwrap();
            assert_eq!(
                trusted(Path::new("/")),
                root_dir.uid() == 0 && root_dir.mode() & 0o022 == 0
            );
        } else {
            assert!(!trusted(Path::new("/")), "nothing is trusted as root");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn programs_resolve_only_through_trusted_locations() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("path");
        let (writable, system) = (root.join("home-bin"), root.join("usr-bin"));
        for d in [&writable, &system] {
            std::fs::create_dir_all(d).unwrap();
            let prog = d.join("prog");
            std::fs::write(&prog, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(system.join("data"), "").unwrap();
        let trusted = |p: &Path| p.starts_with(&system);
        let path_var = format!("{}:relative:{}", writable.display(), system.display());
        assert_eq!(
            resolve_program("prog", &path_var, &trusted),
            Some(system.join("prog"))
        );
        assert_eq!(resolve_program("data", &path_var, &trusted), None);
        assert_eq!(resolve_program("missing", &path_var, &trusted), None);
        let absolute = writable.join("prog");
        assert_eq!(
            resolve_program(absolute.to_str().unwrap(), "", &trusted),
            None
        );
        assert_eq!(
            resolve_program(absolute.to_str().unwrap(), "", &|_| true),
            Some(absolute)
        );
        assert_eq!(resolve_program("home-bin/prog", "", &|_| true), None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
