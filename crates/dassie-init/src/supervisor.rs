//! Runs and supervises the entrypoint (ADR 0005).
//!
//! The supervisor owns no process-wide state: it never blocks signals or calls
//! `waitpid(-1)`. Stop requests and child exits arrive as [`Control`]
//! messages, so the PID-1 layer feeds it from a sigwait/reaper thread and
//! tests feed it directly. That keeps it runnable as an ordinary process.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use dassie_proto::{Event, ExitStatus, GuestState, HealthStatus};
use nix::errno::Errno;
use nix::sys::signal::{SigSet, Signal, kill};
use nix::sys::wait::WaitStatus;
use nix::unistd::Pid;

use crate::config::RunSpec;
use crate::control::{self, ControlListener};
use crate::reporter::{Connector, Reporter};

/// How long the supervisor waits for `exited` to reach the host.
const EXIT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// Inputs to the supervisor loop.
#[derive(Debug)]
pub enum Control {
    /// Stop the entrypoint: SIGTERM now, SIGKILL after `timeout` (the spec's
    /// `stop_timeout` when `None`). Ignored once stopping. `reply`, if given,
    /// receives the status after the request is handled.
    Stop {
        timeout: Option<Duration>,
        reply: Option<mpsc::Sender<HealthStatus>>,
    },
    /// A child process was reaped. Exits of PIDs other than the entrypoint
    /// (orphans reparented to PID 1) are ignored.
    ChildExited { pid: u32, status: ExitStatus },
    /// The readiness probe succeeded. Sent by the supervisor's own probe.
    Ready,
}

/// What the supervisor needs from its environment.
pub struct Deps {
    /// Opens the events connection to the host.
    pub connect: Connector,
    /// Accepts control connections from the host.
    pub control_listener: Box<dyn ControlListener>,
    /// The channel the supervisor reads [`Control`] messages from. Keep clones
    /// of the sender to deliver stop requests and child exits.
    pub control: (mpsc::Sender<Control>, mpsc::Receiver<Control>),
    /// Called with the entrypoint PID right after spawning, before anything
    /// else happens.
    pub on_spawn: Box<dyn FnOnce(u32) + Send>,
}

impl fmt::Debug for Deps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps").finish_non_exhaustive()
    }
}

/// Spawns the entrypoint and supervises it until it exits. Returns its exit
/// status once `exited` has been reported, or has had its chance to be.
pub fn run(spec: &RunSpec, deps: Deps) -> io::Result<ExitStatus> {
    let (control_tx, control_rx) = deps.control;
    let reporter = Reporter::start(deps.connect);

    let pid = spawn(spec)?;
    (deps.on_spawn)(pid);
    reporter.send(Event::Started { pid });

    // With no readiness port, `ready` follows `started` directly, so a
    // fast-exiting entrypoint still reports it before `exited`.
    let mut state = if spec.ready_port.is_some() {
        GuestState::Starting
    } else {
        reporter.send(Event::Ready);
        GuestState::Ready
    };
    let status = Arc::new(Mutex::new(HealthStatus { state, pid }));
    control::serve(
        deps.control_listener,
        Arc::clone(&status),
        control_tx.clone(),
    );

    let probe_done = Arc::new(AtomicBool::new(false));
    if let Some(port) = spec.ready_port {
        probe(port, control_tx.clone(), Arc::clone(&probe_done));
    }

    let set_state = |state| {
        let mut s = status.lock().unwrap_or_else(PoisonError::into_inner);
        s.state = state;
        *s
    };
    let mut kill_at: Option<Instant> = None;

    let exit = loop {
        // Check the deadline first so a stream of messages cannot postpone
        // SIGKILL.
        if kill_at.is_some_and(|at| Instant::now() >= at) {
            signal(pid, Signal::SIGKILL);
            kill_at = None;
        }
        let msg = match kill_at {
            Some(at) => control_rx.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => control_rx
                .recv()
                .map_err(|_| RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(Control::Ready) => {
                if state == GuestState::Starting {
                    state = GuestState::Ready;
                    set_state(state);
                    reporter.send(Event::Ready);
                }
            }
            Ok(Control::Stop { timeout, reply }) => {
                if matches!(state, GuestState::Starting | GuestState::Ready) {
                    state = GuestState::Stopping;
                    set_state(state);
                    signal(pid, Signal::SIGTERM);
                    // A grace period too long for `Instant` means no SIGKILL.
                    kill_at = Instant::now().checked_add(timeout.unwrap_or(spec.stop_timeout));
                }
                if let Some(reply) = reply {
                    let _ = reply.send(set_state(state));
                }
            }
            Ok(Control::ChildExited {
                pid: exited,
                status,
            }) if exited == pid => break status,
            // Orphan exits are ignored; the deadline is handled at the top of
            // the loop.
            Ok(Control::ChildExited { .. }) | Err(RecvTimeoutError::Timeout) => {}
            // We hold a sender ourselves, so this cannot happen.
            Err(RecvTimeoutError::Disconnected) => unreachable!("control channel closed"),
        }
    };

    probe_done.store(true, Ordering::Relaxed);
    set_state(GuestState::Exited);
    reporter.send(Event::Exited { status: exit });
    if !reporter.flush(EXIT_FLUSH_TIMEOUT) {
        eprintln!("dassie-init: could not deliver exited event to the host");
    }
    Ok(exit)
}

/// Converts a reaped wait status into the child PID and its exit status.
/// Returns `None` for stop/continue notifications.
pub fn exit_status(status: WaitStatus) -> Option<(u32, ExitStatus)> {
    match status {
        WaitStatus::Exited(pid, code) => {
            Some((u32::try_from(pid.as_raw()).ok()?, ExitStatus::Code(code)))
        }
        WaitStatus::Signaled(pid, sig, _) => Some((
            u32::try_from(pid.as_raw()).ok()?,
            ExitStatus::Signal(sig as i32),
        )),
        _ => None,
    }
}

fn spawn(spec: &RunSpec) -> io::Result<u32> {
    let (program, args) = spec
        .argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&spec.workdir)
        .stdin(Stdio::null());
    // As root, std also clears supplementary groups before switching.
    if let Some((uid, gid)) = spec.user {
        cmd.uid(uid).gid(gid);
    }
    clear_signal_mask_on_exec(&mut cmd);
    // The child is reaped by whoever feeds `Control::ChildExited`, never via
    // `Child::wait`; dropping the handle neither waits nor kills.
    Ok(cmd.spawn()?.id())
}

/// Makes the child start with no signals blocked.
///
/// A child inherits the signal mask of the thread that spawns it, and std does
/// not reset it. PID 1 blocks SIGTERM, SIGINT and SIGCHLD to receive them with
/// `sigwait`, so without this the entrypoint would start with SIGTERM blocked
/// and every graceful stop would end in SIGKILL. Clearing the mask in the
/// child after `fork` leaves the supervisor's own threads untouched.
///
/// This is the crate's only `unsafe`: std has no safe way to run code between
/// `fork` and `exec`.
#[allow(unsafe_code)]
fn clear_signal_mask_on_exec(cmd: &mut Command) {
    // SAFETY: the closure runs in the forked child before `exec`, where only
    // async-signal-safe operations are allowed. It calls `sigemptyset` and
    // `pthread_sigmask`, both async-signal-safe; it captures nothing and
    // does not allocate (`Errno` converts to `io::Error` without allocating).
    unsafe {
        cmd.pre_exec(|| Ok(SigSet::empty().thread_set_mask()?));
    }
}

fn signal(pid: u32, sig: Signal) {
    let Ok(raw) = i32::try_from(pid) else {
        eprintln!("dassie-init: pid {pid} out of range");
        return;
    };
    match kill(Pid::from_raw(raw), sig) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(e) => eprintln!("dassie-init: kill {pid} {sig}: {e}"),
    }
}

/// Polls a TCP connect to the port inside the guest until it succeeds or the
/// supervisor is done.
fn probe(port: u16, control: mpsc::Sender<Control>, done: Arc<AtomicBool>) {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            if TcpStream::connect_timeout(&addr, PROBE_INTERVAL).is_ok() {
                // The supervisor may already be done; nothing to do then.
                drop(control.send(Control::Ready));
                return;
            }
            thread::sleep(PROBE_INTERVAL);
        }
    });
}
