//! App focus mode: the per-viewer window-list session (`docs/architecture/app-focus-mode.md`).
//!
//! A worker thread samples the windows through a [`Backend`] while the client's switcher or
//! focus mode is open, and focuses windows on request; Win32 is `window_list_capture.rs`. The
//! main loop owns the [`Session`]: it routes client controls in and sends what [`Session::tick`]
//! hands back, at most a few small messages per tick. Nothing here blocks the main loop.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender as Tx};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::capture::MonitorInfo;
use crate::window_list::{
    build, focus_failed_message, icon_message, ClientControl, Entry, FocusFailure, RawWindow,
    Sender, SAMPLE_INTERVAL,
};

/// One look at the desktop: windows in z-order (topmost first), the foreground window and the
/// monitors (selectors are the monitors' device names, as capture uses them).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub windows: Vec<RawWindow>,
    pub foreground: u32,
    pub monitors: Vec<MonitorInfo>,
}

/// What the worker needs from the platform.
pub trait Backend: Send + 'static {
    fn sample(&mut self) -> Snapshot;
    /// Restore and raise the window; on success, the device name of the monitor it is on.
    fn focus(&mut self, id: u32) -> Result<Option<String>, FocusFailure>;
    /// The PNG of an icon hash that an earlier sample reported.
    fn icon(&mut self, hash: &str) -> Option<Vec<u8>>;
}

enum Command {
    Active(bool),
    Focus(u32),
    Icon(String),
}

enum Update {
    List(Vec<Entry>),
    Focused(u32, Result<Option<String>, FocusFailure>),
    Icon(String, Vec<u8>),
}

/// What the main loop does after a tick.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// Messages to send, in order.
    pub messages: Vec<String>,
    /// A window was focused on this monitor (device name): switch the stream there unless it
    /// already shows it.
    pub switch_to: Option<String>,
}

/// One viewer's window-list session.
pub struct Session {
    commands: Option<Tx<Command>>,
    updates: Receiver<Update>,
    sender: Sender,
    worker: Option<JoinHandle<()>>,
}

impl Session {
    pub fn start(backend: impl Backend) -> Self {
        let (commands, inbox) = mpsc::channel();
        let (outbox, updates) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("window-list".into())
            .spawn(move || run(backend, &inbox, &outbox))
            .ok();
        Self {
            commands: Some(commands),
            updates,
            sender: Sender::default(),
            worker,
        }
    }

    fn command(&self, command: Command) {
        if let Some(commands) = &self.commands {
            if commands.send(command).is_err() {
                tracing::debug!("window list: the worker has stopped");
            }
        }
    }

    /// Route one client control. Returns messages to send at once.
    pub fn on_control(&mut self, control: ClientControl) -> Vec<String> {
        match control {
            ClientControl::Start => {
                self.sender.start();
                self.command(Command::Active(true));
            }
            ClientControl::Stop => {
                self.sender.stop();
                self.command(Command::Active(false));
            }
            ClientControl::Focus { id } => {
                if !self.sender.listed(id) {
                    return vec![focus_failed_message(id, FocusFailure::Gone)];
                }
                self.command(Command::Focus(id));
            }
            ClientControl::Icon { hash } => self.command(Command::Icon(hash)),
        }
        Vec::new()
    }

    /// Collect what the worker produced since the last tick. Never blocks.
    pub fn tick(&mut self, now: Instant) -> Tick {
        let mut tick = Tick::default();
        while let Ok(update) = self.updates.try_recv() {
            match update {
                Update::List(entries) => {
                    if let Some(message) = self.sender.next(now, entries) {
                        tick.messages.push(message);
                    }
                }
                Update::Focused(_, Ok(monitor)) => tick.switch_to = monitor,
                Update::Focused(id, Err(reason)) => {
                    tick.messages.push(focus_failed_message(id, reason));
                }
                Update::Icon(hash, png) => tick.messages.push(icon_message(&hash, &png)),
            }
        }
        tick
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Closing the command channel ends the worker at its next wait.
        self.commands = None;
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::warn!("window list: the worker panicked");
            }
        }
    }
}

/// How long an idle worker waits for a command before looking again.
const IDLE_WAIT: Duration = Duration::from_secs(60);

fn run(mut backend: impl Backend, inbox: &Receiver<Command>, outbox: &Tx<Update>) {
    let mut active = false;
    let mut due = Instant::now();
    loop {
        let wait = if active {
            due.saturating_duration_since(Instant::now())
        } else {
            IDLE_WAIT
        };
        match inbox.recv_timeout(wait) {
            Ok(Command::Active(on)) => {
                active = on;
                due = Instant::now();
            }
            Ok(Command::Focus(id)) => {
                let result = backend.focus(id);
                if outbox.send(Update::Focused(id, result)).is_err() {
                    return;
                }
                // Show the raised window's new state at once.
                due = Instant::now();
            }
            Ok(Command::Icon(hash)) => {
                if let Some(png) = backend.icon(&hash) {
                    if outbox.send(Update::Icon(hash, png)).is_err() {
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if active && Instant::now() >= due {
            let snapshot = backend.sample();
            let entries = build(&snapshot.windows, snapshot.foreground, &snapshot.monitors);
            if outbox.send(Update::List(entries)).is_err() {
                return;
            }
            due = Instant::now() + SAMPLE_INTERVAL;
        }
    }
}

#[cfg(test)]
mod tests;
