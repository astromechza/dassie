//! vm-run's main loop against a fake Firecracker: the test plays the guest
//! over real Unix sockets, and receives the journal and notify datagrams on
//! sockets of its own.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use dassie_proto::{ExitStatus, FrameReader, FrameWriter, GuestState, HealthStatus};
use tempfile::TempDir;

use super::outcome::{NO_EXITED_EVENT, VM_FAILED};
use super::*;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Held by tests that spawn a process, and by tests that need a closed
/// socket to stay closed. macOS has no `SOCK_CLOEXEC`, so std marks a new
/// socket close-on-exec only after creating it. A spawn on another test
/// thread in between hands the socket to the child, which keeps a "stale"
/// socket live. Linux creates sockets close-on-exec atomically.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

fn spawn_lock() -> std::sync::MutexGuard<'static, ()> {
    SPAWN_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A Firecracker that exits when the test says so, or when killed.
#[derive(Clone, Default)]
struct FakeVm {
    exit: Arc<Mutex<Option<process::ExitStatus>>>,
    killed: Arc<AtomicBool>,
}

impl FakeVm {
    /// Exits with a raw wait status, such as `3 << 8` for code 3.
    fn exit(&self, raw: i32) {
        *self.exit.lock().unwrap() = Some(process::ExitStatus::from_raw(raw));
    }
}

impl Vm for FakeVm {
    fn try_wait(&mut self) -> io::Result<Option<process::ExitStatus>> {
        Ok(*self.exit.lock().unwrap())
    }

    fn kill(&mut self) -> io::Result<()> {
        self.killed.store(true, Ordering::SeqCst);
        self.exit(9);
        Ok(())
    }
}

struct Harness {
    _dir: TempDir,
    uds: PathBuf,
    journal: UnixDatagram,
    notify: UnixDatagram,
    tx: Sender<Msg>,
    run: JoinHandle<u8>,
}

impl Harness {
    fn start(vm: impl Vm + Send + 'static) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let uds = dir.path().join("vm.vsock");
        let bind = |name: &str| {
            let socket = UnixDatagram::bind(dir.path().join(name)).unwrap();
            socket.set_read_timeout(Some(TIMEOUT)).unwrap();
            socket
        };
        let (journal, notify) = (bind("journal.sock"), bind("notify.sock"));
        let reporting = Reporting {
            journal: Journal::Native(dir.path().join("journal.sock")),
            notify: Notify::new(Some(dir.path().join("notify.sock"))),
        };
        let listener = prepare(&uds).unwrap();
        let (tx, rx) = mpsc::channel();
        let (run_uds, run_tx) = (uds.clone(), tx.clone());
        let mut vm = vm;
        let run = thread::spawn(move || run(&run_uds, listener, &mut vm, &reporting, &run_tx, &rx));
        Self {
            _dir: dir,
            uds,
            journal,
            notify,
            tx,
            run,
        }
    }

    /// Connects as the guest to the events socket.
    fn guest(&self) -> FrameWriter<UnixStream> {
        FrameWriter::new(UnixStream::connect(events_path(&self.uds)).unwrap())
    }

    /// The next journal entry, as text.
    fn journal_entry(&self) -> String {
        let mut buf = [0; 4096];
        let n = self.journal.recv(&mut buf).unwrap();
        String::from_utf8(buf[..n].to_vec()).unwrap()
    }

    fn notification(&self) -> String {
        let mut buf = [0; 4096];
        let n = self.notify.recv(&mut buf).unwrap();
        String::from_utf8(buf[..n].to_vec()).unwrap()
    }

    fn finish(self) -> u8 {
        self.run.join().unwrap()
    }
}

/// Plays Firecracker's side of the control socket for one request: acks the
/// `CONNECT`, answers `stopping` if `answer`, and returns the request it got.
fn serve_control(uds: &Path, answer: bool) -> JoinHandle<Request> {
    let listener = UnixListener::bind(uds).unwrap();
    thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(conn.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "CONNECT 1025\n");
        (&conn).write_all(b"OK 1073741824\n").unwrap();
        let request = FrameReader::new(reader).recv::<Request>().unwrap().unwrap();
        if !answer {
            return request;
        }
        FrameWriter::new(conn)
            .send(&Response::Health(HealthStatus {
                state: GuestState::Stopping,
                pid: 42,
            }))
            .unwrap();
        request
    })
}

#[test]
fn reports_events_and_exits_with_the_apps_code() {
    let vm = FakeVm::default();
    let h = Harness::start(vm.clone());
    let mut guest = h.guest();

    guest.send(&Event::Started { pid: 42 }).unwrap();
    let started = h.journal_entry();
    assert!(started.contains("DASSIE_EVENT=started\n"), "{started}");
    assert!(started.contains("DASSIE_PID=42\n"), "{started}");
    assert!(
        started.contains("SYSLOG_IDENTIFIER=dassie-vm-run\n"),
        "{started}"
    );

    guest.send(&Event::Ready).unwrap();
    assert!(h.journal_entry().contains("DASSIE_EVENT=ready\n"));
    assert_eq!(h.notification(), "READY=1\nSTATUS=ready");

    guest
        .send(&Event::Exited {
            status: ExitStatus::Code(3),
        })
        .unwrap();
    let exited = h.journal_entry();
    assert!(exited.contains("DASSIE_EVENT=exited\n"), "{exited}");
    assert!(exited.contains("DASSIE_CODE=3\n"), "{exited}");

    // The guest reboots: its connection closes and Firecracker exits 0.
    drop(guest);
    vm.exit(0);
    assert_eq!(h.finish(), 3);
}

#[test]
fn a_stop_asks_the_guest_and_exits_zero() {
    // The guest may reboot before it answers.
    for answer in [true, false] {
        stop_asks_the_guest(answer);
    }
}

fn stop_asks_the_guest(answer: bool) {
    let vm = FakeVm::default();
    let h = Harness::start(vm.clone());
    let control = serve_control(&h.uds, answer);
    let mut guest = h.guest();
    guest.send(&Event::Started { pid: 42 }).unwrap();
    h.journal_entry();

    h.tx.send(Msg::Stop).unwrap();
    assert_eq!(control.join().unwrap(), Request::Stop { timeout_ms: None });
    // A second SIGTERM changes nothing.
    h.tx.send(Msg::Stop).unwrap();

    guest
        .send(&Event::Exited {
            status: ExitStatus::Signal(15),
        })
        .unwrap();
    // Firecracker exits on its own, a little later.
    thread::sleep(Duration::from_millis(200));
    drop(guest);
    vm.exit(0);
    assert_eq!(h.finish(), 0);
    assert!(!vm.killed.load(Ordering::SeqCst), "answer={answer}");
}

#[test]
fn a_stop_the_guest_cannot_take_kills_firecracker() {
    // Nothing serves the control socket, as before the guest is up.
    let vm = FakeVm::default();
    let h = Harness::start(vm.clone());
    h.tx.send(Msg::Stop).unwrap();
    assert_eq!(h.finish(), 0);
    assert!(vm.killed.load(Ordering::SeqCst));
}

#[test]
fn no_exited_event_is_reserved_code() {
    let vm = FakeVm::default();
    let h = Harness::start(vm.clone());
    let mut guest = h.guest();
    guest.send(&Event::Started { pid: 42 }).unwrap();
    h.journal_entry();
    drop(guest);
    vm.exit(0);
    assert_eq!(h.finish(), NO_EXITED_EVENT);
}

#[test]
fn an_exited_event_racing_firecracker_exit_is_not_lost() {
    let vm = FakeVm::default();
    let h = Harness::start(vm.clone());
    let mut guest = h.guest();
    guest.send(&Event::Started { pid: 42 }).unwrap();
    h.journal_entry();

    guest
        .send(&Event::Exited {
            status: ExitStatus::Code(3),
        })
        .unwrap();
    vm.exit(0);
    thread::sleep(Duration::from_millis(200));
    drop(guest);
    assert_eq!(h.finish(), 3);
}

#[test]
fn a_failed_firecracker_is_reserved_code() {
    let vm = FakeVm::default();
    let h = Harness::start(vm.clone());
    vm.exit(1 << 8);
    assert_eq!(h.finish(), VM_FAILED);
}

#[test]
fn spawns_a_real_process() {
    let _spawn = spawn_lock();
    // Neither boots a guest, so neither sends an `exited` event.
    for (bin, want) in [
        ("/usr/bin/true", NO_EXITED_EVENT),
        ("/usr/bin/false", VM_FAILED),
    ] {
        let child = spawn_firecracker(Path::new(bin), Path::new("/nonexistent.json")).unwrap();
        assert_eq!(Harness::start(child).finish(), want, "{bin}");
    }
}

#[test]
fn prepare_clears_stale_sockets_but_not_live_ones() {
    let _spawn = spawn_lock();
    let dir = tempfile::tempdir().unwrap();
    let uds = dir.path().join("vm.vsock");
    // A killed Firecracker and a killed vm-run leave both sockets behind.
    drop(UnixListener::bind(&uds).unwrap());
    drop(UnixListener::bind(events_path(&uds)).unwrap());
    let listener = prepare(&uds).unwrap();
    assert!(!uds.exists());

    // A second vm-run for the same VM must not steal the live socket.
    let err = prepare(&uds).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "{err}");
    drop(listener);
}
