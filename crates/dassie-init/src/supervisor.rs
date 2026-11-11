//! Runs and supervises the entrypoint (ADR 0005).
//!
//! The supervisor owns no process-wide state: it never blocks signals or calls
//! `waitpid(-1)`. Stop requests and child exits arrive as [`Control`]
//! messages, so the PID-1 layer feeds it from a sigwait/reaper thread and
//! tests feed it directly. That keeps it runnable as an ordinary process.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use dassie_proto::{Event, ExitStatus, GuestState, HealthStatus};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::WaitStatus;
use nix::unistd::Pid;

use crate::config::RunSpec;
use crate::control::{self, ControlListener};
use crate::reporter::{Connector, Reporter};

/// How long the supervisor waits for `exited` to reach the host.
const EXIT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// Inputs to the supervisor loop.
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

/// Spawns the entrypoint and supervises it until it exits. Returns its exit
/// status once `exited` has been reported, or has had its chance to be.
pub fn run(spec: &RunSpec, deps: Deps) -> io::Result<ExitStatus> {
    let (control_tx, control_rx) = deps.control;
    let reporter = Reporter::start(deps.connect);

    let pid = spawn(spec)?;
    (deps.on_spawn)(pid);
    reporter.send(Event::Started { pid });

    let status = Arc::new(Mutex::new(HealthStatus {
        state: GuestState::Starting,
        pid,
    }));
    control::serve(
        deps.control_listener,
        Arc::clone(&status),
        control_tx.clone(),
    );

    let probe_done = Arc::new(AtomicBool::new(false));
    match spec.ready_port {
        Some(port) => probe(port, control_tx.clone(), Arc::clone(&probe_done)),
        None => {
            let _ = control_tx.send(Control::Ready);
        }
    }

    let set_state = |state| {
        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
        s.state = state;
        *s
    };
    let mut state = GuestState::Starting;
    let mut kill_at: Option<Instant> = None;

    let exit = loop {
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
                    kill_at = Some(Instant::now() + timeout.unwrap_or(spec.stop_timeout));
                }
                if let Some(reply) = reply {
                    let _ = reply.send(set_state(state));
                }
            }
            Ok(Control::ChildExited {
                pid: exited,
                status,
            }) if exited == pid => break status,
            Ok(Control::ChildExited { .. }) => {}
            Err(RecvTimeoutError::Timeout) => {
                signal(pid, Signal::SIGKILL);
                kill_at = None;
            }
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
        WaitStatus::Exited(pid, code) => Some((pid.as_raw() as u32, ExitStatus::Code(code))),
        WaitStatus::Signaled(pid, sig, _) => {
            Some((pid.as_raw() as u32, ExitStatus::Signal(sig as i32)))
        }
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
    // The child is reaped by whoever feeds `Control::ChildExited`, never via
    // `Child::wait`; dropping the handle neither waits nor kills.
    Ok(cmd.spawn()?.id())
}

fn signal(pid: u32, sig: Signal) {
    match kill(Pid::from_raw(pid as i32), sig) {
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
                let _ = control.send(Control::Ready);
                return;
            }
            thread::sleep(PROBE_INTERVAL);
        }
    });
}
