//! CLIPBOARD selection owner for the session's private display.
//!
//! An X selection is served on demand: whenever an app pastes, it asks the
//! owner for the data. The daemon's main thread blocks in X waits and in
//! stdin reads, so the owner runs on its own thread with its own X
//! connection and serves `SelectionRequest`s at any time. Only plain text is
//! offered (`UTF8_STRING`, `STRING`, `TEXT`), never other formats.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::NONE;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        TIMESTAMP,
        TEXT,
        UTF8_STRING,
        _LINUX_CUA_TIME,
    }
}

enum Msg {
    Set(String, Sender<Result<(), String>>),
    Release(Sender<Result<(), String>>),
    Quit,
}

#[derive(Default)]
struct State {
    /// Text this daemon currently offers (it owns the selection).
    text: Option<String>,
}

/// Handle on the owner thread.
pub struct Owner {
    tx: Sender<Msg>,
    wake: Mutex<UnixStream>,
    state: Arc<Mutex<State>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

const REPLY_WAIT: Duration = Duration::from_secs(3);

impl Owner {
    /// Connects a second time to `display` and starts serving.
    pub fn start(display: &str) -> Result<Owner> {
        let (tx, rx) = mpsc::channel();
        let (wake, wake_rx) = UnixStream::pair().context("clipboard wake pipe")?;
        wake_rx.set_nonblocking(true)?;
        let state = Arc::new(Mutex::new(State::default()));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let thread_state = state.clone();
        let display = display.to_string();
        let thread = std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                if let Err(e) = serve(&display, rx, wake_rx, thread_state, ready_tx) {
                    eprintln!("clipboard owner stopped: {e:#}");
                }
            })
            .context("starting the clipboard thread")?;
        match ready_rx.recv_timeout(REPLY_WAIT) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => anyhow::bail!("clipboard owner: {e}"),
            Err(_) => anyhow::bail!("clipboard owner did not start"),
        }
        Ok(Owner {
            tx,
            wake: Mutex::new(wake),
            state,
            thread: Some(thread),
        })
    }

    pub fn alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    fn send(&self, msg: Msg) -> Result<()> {
        self.tx
            .send(msg)
            .map_err(|_| anyhow::anyhow!("the clipboard owner thread has stopped"))?;
        let _ = self.wake.lock().expect("wake pipe").write_all(&[1]);
        Ok(())
    }

    fn ask(&self, make: impl FnOnce(Sender<Result<(), String>>) -> Msg) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.send(make(tx))?;
        match rx.recv_timeout(REPLY_WAIT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => anyhow::bail!("clipboard: {e}"),
            Err(_) => anyhow::bail!("the clipboard owner did not answer"),
        }
    }

    /// Takes the selection with `text`.
    pub fn set(&self, text: &str) -> Result<()> {
        let text = text.to_string();
        self.ask(|tx| Msg::Set(text, tx))
    }

    /// Gives the selection up (nobody owns it afterwards).
    pub fn release(&self) -> Result<()> {
        self.ask(Msg::Release)
    }

    /// The text this daemon serves, if it owns the selection.
    pub fn owned_text(&self) -> Option<String> {
        self.state.lock().expect("clipboard state").text.clone()
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.send(Msg::Quit);
    }
}

fn server_time(conn: &RustConnection, win: Window, atom: xproto::Atom) -> Result<u32> {
    conn.change_property8(PropMode::APPEND, win, atom, AtomEnum::STRING, &[])?;
    conn.flush()?;
    loop {
        if let Event::PropertyNotify(e) = conn.wait_for_event()? {
            if e.window == win && e.atom == atom {
                return Ok(e.time);
            }
        }
    }
}

fn serve(
    display: &str,
    rx: Receiver<Msg>,
    mut wake: UnixStream,
    state: Arc<Mutex<State>>,
    ready: Sender<Result<(), String>>,
) -> Result<()> {
    let setup = (|| -> Result<(RustConnection, Window, Atoms)> {
        let (conn, screen) = x11rb::connect(Some(display))
            .with_context(|| format!("connecting to X display {display}"))?;
        let root = conn.setup().roots[screen].root;
        let win = conn.generate_id()?;
        conn.create_window(
            0,
            win,
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
        .context("creating the clipboard window")?;
        let atoms = Atoms::new(&conn)?.reply()?;
        Ok((conn, win, atoms))
    })();
    let (conn, win, atoms) = match setup {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready.send(Err(format!("{e:#}")));
            return Err(e);
        }
    };
    let mut text: Option<String> = None;
    let mut own_time: u32 = 0;
    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Set(t, reply) => {
                    let r = (|| -> Result<()> {
                        own_time = server_time(&conn, win, atoms._LINUX_CUA_TIME)?;
                        conn.set_selection_owner(win, atoms.CLIPBOARD, own_time)?;
                        let owner = conn.get_selection_owner(atoms.CLIPBOARD)?.reply()?.owner;
                        if owner != win {
                            anyhow::bail!("could not take ownership of the CLIPBOARD selection");
                        }
                        Ok(())
                    })();
                    match &r {
                        Ok(()) => {
                            text = Some(t.clone());
                            state.lock().expect("clipboard state").text = Some(t);
                        }
                        Err(_) => {
                            text = None;
                            state.lock().expect("clipboard state").text = None;
                        }
                    }
                    let _ = reply.send(r.map_err(|e| format!("{e:#}")));
                }
                Msg::Release(reply) => {
                    let r = (|| -> Result<()> {
                        if text.is_some() {
                            conn.set_selection_owner(NONE, atoms.CLIPBOARD, own_time)?;
                            conn.flush()?;
                        }
                        Ok(())
                    })();
                    text = None;
                    state.lock().expect("clipboard state").text = None;
                    let _ = reply.send(r.map_err(|e| format!("{e:#}")));
                }
                Msg::Quit => return Ok(()),
            }
        }
        conn.flush()?;
        while let Some(ev) = conn.poll_for_event()? {
            match ev {
                Event::SelectionRequest(e) if e.owner == win => {
                    let property = if e.property == NONE {
                        e.target
                    } else {
                        e.property
                    };
                    let served = match &text {
                        Some(_) if e.target == atoms.TARGETS => conn
                            .change_property32(
                                PropMode::REPLACE,
                                e.requestor,
                                property,
                                AtomEnum::ATOM,
                                &[
                                    atoms.TARGETS,
                                    atoms.TIMESTAMP,
                                    atoms.UTF8_STRING,
                                    atoms.TEXT,
                                    u32::from(AtomEnum::STRING),
                                ],
                            )
                            .is_ok(),
                        Some(_) if e.target == atoms.TIMESTAMP => conn
                            .change_property32(
                                PropMode::REPLACE,
                                e.requestor,
                                property,
                                AtomEnum::INTEGER,
                                &[own_time],
                            )
                            .is_ok(),
                        Some(t) if e.target == atoms.UTF8_STRING || e.target == atoms.TEXT => conn
                            .change_property8(
                                PropMode::REPLACE,
                                e.requestor,
                                property,
                                atoms.UTF8_STRING,
                                t.as_bytes(),
                            )
                            .is_ok(),
                        Some(t) if e.target == u32::from(AtomEnum::STRING) => {
                            let latin1: Vec<u8> = t
                                .chars()
                                .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
                                .collect();
                            conn.change_property8(
                                PropMode::REPLACE,
                                e.requestor,
                                property,
                                AtomEnum::STRING,
                                &latin1,
                            )
                            .is_ok()
                        }
                        _ => false,
                    };
                    let notify = xproto::SelectionNotifyEvent {
                        response_type: xproto::SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: e.time,
                        requestor: e.requestor,
                        selection: e.selection,
                        target: e.target,
                        property: if served { property } else { NONE },
                    };
                    let _ = conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify);
                    conn.flush()?;
                }
                Event::SelectionClear(e) if e.owner == win => {
                    // Another client took the selection over.
                    text = None;
                    state.lock().expect("clipboard state").text = None;
                }
                _ => {}
            }
        }
        // Sleep until the X connection or the wake pipe has data.
        let mut fds = [
            libc::pollfd {
                fd: conn.stream().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, 1000) };
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err).context("poll in the clipboard thread");
            }
        }
        if fds[1].revents != 0 {
            let mut buf = [0u8; 64];
            match wake.read(&mut buf) {
                Ok(0) => return Ok(()), // the owner handle is gone
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e).context("clipboard wake pipe"),
            }
        }
    }
}
