//! `dassie vm-run`: runs one boot of one VM as a systemd unit's main process
//! (ADR 0012).
//!
//! vm-run owns the VM's vsock sockets. It turns the guest's best-effort
//! events into state systemd keeps: journal entries, `READY=1`, and its own
//! exit status. A SIGTERM becomes a graceful `stop` request. Sync and
//! portable: only the binary's `main` touches the process environment and
//! signals.

mod events;
mod journal;
mod notify;
mod outcome;

use std::io;
use std::os::unix::net::{UnixDatagram, UnixListener};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use dassie_proto::{EVENTS_PORT, Event, Request, Response};
use dassie_vm::{ControlClient, guest_port_path, remove_stale_socket, vsock_uds_path};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

pub(crate) use journal::Journal;
pub(crate) use notify::Notify;
use outcome::VM_FAILED;

/// How often the loop checks whether Firecracker has exited.
const POLL: Duration = Duration::from_millis(100);

/// Bounds each read and write of the `stop` request.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to wait for Firecracker after a `stop`: the guest's default
/// grace period (10 s) plus time to reboot.
const STOP_WAIT: Duration = Duration::from_secs(15);

/// How long to wait, once Firecracker has exited, for the events connection
/// to reach EOF.
const DRAIN: Duration = Duration::from_secs(1);

/// How long to wait, once Firecracker has exited, for a guest connection the
/// events thread has not reported yet, when it has reported none.
const ACCEPT_GRACE: Duration = Duration::from_millis(200);

/// Messages the main loop may be behind by. A guest writing events faster
/// than vm-run reports them is held back, so it cannot grow vm-run's memory.
const QUEUE: usize = 64;

/// Bounds each journal or notify datagram, so a stuck journald or service
/// manager cannot stall the main loop.
const SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// What the main loop hears about.
#[derive(Debug)]
pub(crate) enum Msg {
    /// A guest connected to the events socket.
    Connected,
    /// A guest event arrived.
    Event(Event),
    /// The events connection closed.
    Disconnected,
    /// systemd (or a terminal) asked vm-run to stop.
    Stop,
}

/// The running VM: Firecracker, or a fake in tests.
pub(crate) trait Vm {
    fn try_wait(&mut self) -> io::Result<Option<process::ExitStatus>>;
    fn kill(&mut self) -> io::Result<()>;
}

impl Vm for Child {
    fn try_wait(&mut self) -> io::Result<Option<process::ExitStatus>> {
        Child::try_wait(self)
    }

    fn kill(&mut self) -> io::Result<()> {
        Child::kill(self)
    }
}

/// Where vm-run reports to.
#[derive(Debug, Clone)]
pub(crate) struct Reporting {
    pub(crate) journal: Journal,
    pub(crate) notify: Notify,
}

/// Runs `dassie vm-run` and returns its exit status.
pub(crate) fn vm_run(config: &Path, firecracker: &Path) -> u8 {
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    // First, so a SIGTERM during setup is a stop, not a kill.
    if let Err(e) = forward_signals(tx.clone()) {
        eprintln!("vm-run: install signal handlers: {e}");
        return VM_FAILED;
    }
    let reporting = Reporting {
        journal: Journal::from_env(),
        notify: Notify::from_env(),
    };
    let prepared = vsock_uds_path(config).and_then(|uds| {
        let listener = prepare(&uds)?;
        Ok((uds, listener))
    });
    let (uds, listener) = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            eprintln!("vm-run: {e}");
            return VM_FAILED;
        }
    };
    let mut child = match spawn_firecracker(firecracker, config) {
        Ok(child) => child,
        Err(e) => {
            eprintln!("vm-run: spawn {}: {e}", firecracker.display());
            return VM_FAILED;
        }
    };
    run(&uds, listener, &mut child, &reporting, &tx, &rx)
}

/// Sends a [`Msg::Stop`] for every SIGTERM or SIGINT.
fn forward_signals(tx: SyncSender<Msg>) -> io::Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGINT])?;
    thread::spawn(move || {
        for _ in signals.forever() {
            if tx.send(Msg::Stop).is_err() {
                return;
            }
        }
    });
    Ok(())
}

/// Clears what a previous boot left behind, then listens for the guest's
/// events, before Firecracker starts.
pub(crate) fn prepare(uds: &Path) -> io::Result<UnixListener> {
    let events = events_path(uds);
    remove_stale_socket(uds)?;
    remove_stale_socket(&events)?;
    UnixListener::bind(&events)
        .map_err(|e| io::Error::new(e.kind(), format!("bind {}: {e}", events.display())))
}

/// Sends one datagram to `socket`, waiting at most [`SEND_TIMEOUT`].
pub(crate) fn send_datagram(socket: &Path, bytes: &[u8]) -> io::Result<()> {
    let sender = UnixDatagram::unbound()?;
    sender.set_write_timeout(Some(SEND_TIMEOUT))?;
    sender.send_to(bytes, socket).map(drop)
}

/// The host socket the guest's events arrive on.
fn events_path(uds: &Path) -> PathBuf {
    guest_port_path(uds, EVENTS_PORT)
}

pub(crate) fn spawn_firecracker(firecracker: &Path, config: &Path) -> io::Result<Child> {
    // --no-api: the config file is the whole VM, and there is no API socket
    // to collide on.
    Command::new(firecracker)
        .arg("--no-api")
        .arg("--config-file")
        .arg(config)
        .spawn()
}

/// The main loop: reports events until Firecracker exits, handling a stop
/// on the way, and returns the exit status.
pub(crate) fn run(
    uds: &Path,
    listener: UnixListener,
    vm: &mut dyn Vm,
    reporting: &Reporting,
    tx: &SyncSender<Msg>,
    rx: &Receiver<Msg>,
) -> u8 {
    events::spawn(listener, tx.clone());
    let mut state = State::default();
    let mut kill_at = None;
    let firecracker = loop {
        match rx.recv_timeout(POLL) {
            Ok(Msg::Stop) if !state.stopped => {
                state.stopped = true;
                if request_stop(uds) {
                    kill_at = Some(Instant::now() + STOP_WAIT);
                } else {
                    eprintln!("vm-run: the guest cannot stop itself; killing firecracker");
                    kill(vm);
                }
            }
            Ok(msg) => state.handle(msg, Some(reporting)),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
        if kill_at.is_some_and(|at| Instant::now() >= at) {
            eprintln!("vm-run: firecracker still running {STOP_WAIT:?} after stop; killing it");
            kill(vm);
            kill_at = None;
        }
        match vm.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                eprintln!("vm-run: wait for firecracker: {e}");
                return VM_FAILED;
            }
        }
    };

    // The guest may have written `exited` just before it rebooted, on a
    // connection the events thread has not reported yet. With none reported
    // this boot, wait briefly for one.
    let start = Instant::now();
    loop {
        let late = start.elapsed() >= DRAIN;
        let wait = if late {
            None
        } else if state.connected {
            Some(start + DRAIN)
        } else if !state.seen {
            Some(start + ACCEPT_GRACE)
        } else {
            // Take what is already queued, such as a reconnection, without
            // waiting for more.
            None
        };
        let msg = match wait {
            Some(until) => rx
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .ok(),
            None => rx.try_recv().ok(),
        };
        let Some(msg) = msg else { break };
        // Past the deadline, only record what the guest said, so a queued
        // `exited` still sets the exit status but a stalled journal cannot
        // hold up the exit.
        state.handle(msg, (!late).then_some(reporting));
    }

    let (code, reason) = outcome::exit_code(state.stopped, state.exited, firecracker);
    if let Some(reason) = reason {
        eprintln!("vm-run: {reason}; exiting {code}");
    }
    code
}

#[derive(Debug, Default)]
struct State {
    stopped: bool,
    connected: bool,
    /// Whether any events connection was reported this boot.
    seen: bool,
    exited: Option<dassie_proto::ExitStatus>,
}

impl State {
    /// Updates the state, and reports events unless `reporting` is `None`.
    fn handle(&mut self, msg: Msg, reporting: Option<&Reporting>) {
        match msg {
            Msg::Connected => {
                self.connected = true;
                self.seen = true;
            }
            Msg::Disconnected => self.connected = false,
            Msg::Stop => {}
            Msg::Event(event) => {
                if let Event::Exited { status } = &event {
                    self.exited = Some(*status);
                }
                let Some(reporting) = reporting else { return };
                if let Some(entry) = outcome::entry(&event) {
                    reporting.journal.send(&entry);
                }
                if event == Event::Ready {
                    reporting.notify.send(outcome::READY);
                }
            }
        }
    }
}

/// Asks the guest to stop its entrypoint, with the guest's default grace
/// period. Returns whether the request may have reached the guest.
///
/// Only a failed connect, or an `error` answer, means the guest is not
/// stopping. A guest whose entrypoint exits at once on SIGTERM can reboot
/// before its answer is written, so a connection closed after the request
/// went out still counts as stopping.
fn request_stop(uds: &Path) -> bool {
    let mut client = match ControlClient::connect(uds, CONTROL_TIMEOUT) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("vm-run: stop request: {e}");
            return false;
        }
    };
    match client.request(&Request::Stop { timeout_ms: None }) {
        Ok(Response::Health(_)) => true,
        Ok(other) => {
            eprintln!("vm-run: stop request: guest answered {other:?}");
            false
        }
        Err(e) => {
            eprintln!("vm-run: stop request: {e}; waiting for firecracker");
            true
        }
    }
}

fn kill(vm: &mut dyn Vm) {
    if let Err(e) = vm.kill() {
        eprintln!("vm-run: kill firecracker: {e}");
    }
}

#[cfg(test)]
mod tests;
