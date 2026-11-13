//! The Linux-only PID-1 layer (ADR 0005, ADR 0006).
//!
//! Sets up the guest from the shared initramfs, switches into the app rootfs
//! while staying PID 1, turns signals and child exits into supervisor
//! [`Control`] messages, and reboots the VM once the entrypoint exits.
//!
//! PID 1 must never exit: the kernel panics if it does. Every outcome,
//! including errors and panics, ends in [`shutdown`].

mod sys;

use std::error::Error;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{fs, panic, thread};

use dassie_init::boot::RootSpec;
use dassie_init::config;
use dassie_init::supervisor::{self, Control, Deps};
use dassie_proto::{CONTROL_PORT, EVENTS_PORT, ExitStatus};
use nix::errno::Errno;
use nix::sys::reboot::{self, RebootMode};
use nix::sys::signal::{SigSet, Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, sync};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub(crate) fn main() -> ! {
    match panic::catch_unwind(run) {
        Ok(Ok(status)) => eprintln!("dassie-init: entrypoint exited with {status:?}"),
        Ok(Err(e)) => eprintln!("dassie-init: fatal: {e}"),
        // The panic hook has already printed the message.
        Err(_) => eprintln!("dassie-init: fatal: panic"),
    }
    shutdown()
}

fn run() -> Result<ExitStatus> {
    // Block before any thread exists so every thread inherits the mask and
    // only the sigwait thread sees these. The supervisor spawns the
    // entrypoint with an empty mask.
    let signals = block_signals()?;

    sys::early()?;
    sys::reopen_console()?;
    let cmdline =
        fs::read_to_string("/proc/cmdline").map_err(|e| format!("read /proc/cmdline: {e}"))?;
    let root = RootSpec::from_cmdline(&cmdline);
    sys::switch_root(&root)?;
    sys::dev_extras()?;

    sys::interface_up("lo")?;
    // A NIC is optional; the kernel `ip=` arg has already set its address.
    if Path::new("/sys/class/net/eth0").exists() {
        sys::interface_up("eth0")?;
    }
    // Ctrl-Alt-Del becomes SIGINT to PID 1 instead of an immediate reboot.
    reboot::set_cad_enabled(false).map_err(|e| format!("disable Ctrl-Alt-Del: {e}"))?;

    let spec = config::load(Path::new("/"), &cmdline)?;
    if let Some((uid, gid)) = spec.user {
        sys::give_console(uid, gid)?;
    }
    eprintln!("dassie-init: starting {:?}", spec.argv);

    let (control_tx, control_rx) = mpsc::channel();
    handle_signals(signals, control_tx.clone());
    let deps = Deps {
        connect: Box::new(|| sys::vsock_connect(sys::HOST_CID, EVENTS_PORT)),
        control_listener: Box::new(sys::VsockListener::bind(CONTROL_PORT)?),
        control: (control_tx, control_rx),
        on_spawn: Box::new(|pid| eprintln!("dassie-init: entrypoint pid {pid}")),
    };
    Ok(supervisor::run(&spec, deps)?)
}

/// Kills whatever the entrypoint left behind, flushes the filesystems and
/// reboots the VM, which makes Firecracker exit. Never returns.
fn shutdown() -> ! {
    // kill(-1) reaches every process except PID 1 itself.
    match kill(Pid::from_raw(-1), Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(e) => eprintln!("dassie-init: kill stragglers: {e}"),
    }
    wait_for_children(STRAGGLER_TIMEOUT);
    sync();
    let Err(e) = reboot::reboot(RebootMode::RB_AUTOBOOT);
    eprintln!("dassie-init: reboot: {e}");
    // Exiting would panic the kernel; wait for the host to kill the VM.
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

/// How long shutdown waits for killed processes to die before `sync`.
const STRAGGLER_TIMEOUT: Duration = Duration::from_secs(1);

/// Waits until PID 1 has no children left, or `timeout` passes, so a process
/// that was still writing cannot dirty the filesystem after `sync`. Every
/// userspace process is a descendant of PID 1, and orphans are reparented to
/// it, so ECHILD means they are all gone. The sigwait thread may reap some of
/// them first; either way they are gone.
fn wait_for_children(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Err(Errno::ECHILD) => return,
            Ok(WaitStatus::StillAlive) => thread::sleep(Duration::from_millis(10)),
            Ok(_) | Err(Errno::EINTR) => {}
            Err(e) => {
                eprintln!("dassie-init: waitpid: {e}");
                return;
            }
        }
    }
    eprintln!("dassie-init: processes still running after SIGKILL; syncing anyway");
}

fn block_signals() -> Result<SigSet> {
    let mut set = SigSet::empty();
    set.add(Signal::SIGTERM);
    set.add(Signal::SIGINT);
    set.add(Signal::SIGCHLD);
    set.thread_block()
        .map_err(|e| format!("block signals: {e}"))?;
    Ok(set)
}

/// Receives the blocked signals with `sigwait` on a background thread:
/// SIGCHLD reaps every exited child, SIGTERM and SIGINT request a stop.
fn handle_signals(set: SigSet, control: mpsc::Sender<Control>) {
    thread::spawn(move || {
        loop {
            match set.wait() {
                Ok(Signal::SIGCHLD) => reap(&control),
                Ok(sig) => {
                    eprintln!("dassie-init: {sig}, stopping");
                    // Fails only once the supervisor is done.
                    drop(control.send(Control::Stop {
                        timeout: None,
                        reply: None,
                    }));
                }
                Err(e) => {
                    eprintln!("dassie-init: sigwait: {e}");
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });
}

/// Reaps every exited child. SIGCHLD coalesces, so one signal can stand for
/// several exits. Orphans reparented to PID 1 are reaped here too; the
/// supervisor ignores exits that are not the entrypoint's.
fn reap(control: &mpsc::Sender<Control>) {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => return,
            Ok(status) => {
                if let Some((pid, status)) = supervisor::exit_status(status) {
                    drop(control.send(Control::ChildExited { pid, status }));
                }
            }
            Err(Errno::EINTR) => {}
            Err(e) => {
                eprintln!("dassie-init: waitpid: {e}");
                return;
            }
        }
    }
}
