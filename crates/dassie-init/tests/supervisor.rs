//! Supervisor end-to-end on any Unix: Unix sockets stand in for vsock, and a
//! per-test `waitpid(pid)` thread stands in for PID 1's reaper.

use std::io::{self, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dassie_init::config::RunSpec;
use dassie_init::supervisor::{self, Control, Deps};
use dassie_proto::{
    ErrorCode, Event, ExitStatus, FrameReader, FrameWriter, GuestState, HealthStatus, Request,
    Response,
};
use nix::sys::wait::waitpid;
use nix::unistd::Pid;

const TIMEOUT: Duration = Duration::from_secs(5);

/// A host events listener that can be killed (listener and connections
/// closed) to simulate a host restart.
struct EventsListener {
    events: mpsc::Receiver<Event>,
    stop: Arc<AtomicBool>,
    conns: Arc<Mutex<Vec<UnixStream>>>,
    path: PathBuf,
}

impl EventsListener {
    fn bind(path: &Path) -> Self {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (tx, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let conns = Arc::new(Mutex::new(Vec::<UnixStream>::new()));
        {
            let (stop, conns) = (Arc::clone(&stop), Arc::clone(&conns));
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            conn.set_nonblocking(false).unwrap();
                            conns.lock().unwrap().push(conn.try_clone().unwrap());
                            let tx = tx.clone();
                            thread::spawn(move || {
                                let mut reader = FrameReader::new(BufReader::new(conn));
                                while let Ok(Some(event)) = reader.recv::<Event>() {
                                    let _ = tx.send(event);
                                }
                            });
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                }
            });
        }
        Self {
            events,
            stop,
            conns,
            path: path.to_owned(),
        }
    }

    fn next(&self) -> Event {
        self.events.recv_timeout(TIMEOUT).expect("no event in time")
    }

    fn assert_quiet(&self, for_: Duration) {
        match self.events.recv_timeout(for_) {
            Err(RecvTimeoutError::Timeout) => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    fn kill(self) {
        self.stop.store(true, Ordering::Relaxed);
        for conn in self.conns.lock().unwrap().iter() {
            let _ = conn.shutdown(std::net::Shutdown::Both);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

struct Guest {
    dir: tempfile::TempDir,
    result: JoinHandle<io::Result<ExitStatus>>,
}

impl Guest {
    /// Starts the supervisor for `argv`. Events go to `<dir>/events.sock`,
    /// which the test may or may not be listening on.
    fn start(argv: &[&str], ready_port: Option<u16>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let spec = RunSpec {
            argv: argv.iter().map(|s| (*s).to_owned()).collect(),
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            user: None,
            workdir: "/".into(),
            ready_port,
            stop_timeout: Duration::from_secs(10),
        };
        let events_path = dir.path().join("events.sock");
        let control_listener = UnixListener::bind(dir.path().join("control.sock")).unwrap();
        let (control_tx, control_rx) = mpsc::channel();
        let reaper_tx = control_tx.clone();
        let deps = Deps {
            connect: Box::new(move || {
                UnixStream::connect(&events_path).map(|s| Box::new(s) as Box<dyn Write + Send>)
            }),
            control_listener: Box::new(control_listener),
            control: (control_tx, control_rx),
            on_spawn: Box::new(move |pid| {
                thread::spawn(move || {
                    let status = waitpid(Pid::from_raw(pid as i32), None).unwrap();
                    let (pid, status) = supervisor::exit_status(status).unwrap();
                    let _ = reaper_tx.send(Control::ChildExited { pid, status });
                });
            }),
        };
        let result = thread::spawn(move || supervisor::run(&spec, deps));
        Self { dir, result }
    }

    fn events_path(&self) -> PathBuf {
        self.dir.path().join("events.sock")
    }

    fn listen(&self) -> EventsListener {
        EventsListener::bind(&self.events_path())
    }

    fn control(&self) -> ControlClient {
        let stream = UnixStream::connect(self.dir.path().join("control.sock")).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        ControlClient {
            reader: FrameReader::new(BufReader::new(stream.try_clone().unwrap())),
            writer: FrameWriter::new(stream),
        }
    }

    fn wait(self) -> ExitStatus {
        self.result.join().unwrap().unwrap()
    }
}

struct ControlClient {
    reader: FrameReader<BufReader<UnixStream>>,
    writer: FrameWriter<UnixStream>,
}

impl ControlClient {
    fn request(&mut self, req: &Request) -> Response {
        self.writer.send(req).unwrap();
        self.reader.recv().unwrap().expect("guest closed control")
    }

    fn state(&mut self) -> GuestState {
        match self.request(&Request::Health) {
            Response::Health(HealthStatus { state, .. }) => state,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn stop(timeout_ms: u64) -> Request {
    Request::Stop {
        timeout_ms: Some(timeout_ms),
    }
}

#[test]
fn reports_started_ready_and_exit_code() {
    let guest = Guest::start(&["sh", "-c", "sleep 0.3; exit 3"], None);
    let host = guest.listen();
    assert!(matches!(host.next(), Event::Started { .. }));
    assert_eq!(host.next(), Event::Ready);
    assert_eq!(
        host.next(),
        Event::Exited {
            status: ExitStatus::Code(3)
        }
    );
    assert_eq!(guest.wait(), ExitStatus::Code(3));
}

#[test]
fn ready_waits_for_the_port() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let guest = Guest::start(&["sleep", "30"], Some(port));
    let host = guest.listen();
    assert!(matches!(host.next(), Event::Started { .. }));
    host.assert_quiet(Duration::from_millis(300));
    let mut control = guest.control();
    assert_eq!(control.state(), GuestState::Starting);

    let _app = TcpListener::bind(("127.0.0.1", port)).unwrap();
    assert_eq!(host.next(), Event::Ready);
    assert_eq!(control.state(), GuestState::Ready);

    control.request(&stop(5000));
    assert_eq!(guest.wait(), ExitStatus::Signal(15));
}

#[test]
fn stop_sends_sigterm() {
    let guest = Guest::start(&["sleep", "30"], None);
    let host = guest.listen();
    let Event::Started { pid } = host.next() else {
        panic!("expected started")
    };
    assert_eq!(host.next(), Event::Ready);

    let mut control = guest.control();
    assert_eq!(
        control.request(&stop(5000)),
        Response::Health(HealthStatus {
            state: GuestState::Stopping,
            pid
        })
    );
    assert_eq!(
        host.next(),
        Event::Exited {
            status: ExitStatus::Signal(15)
        }
    );
    assert_eq!(guest.wait(), ExitStatus::Signal(15));
}

#[test]
fn stop_escalates_to_sigkill_without_extending_the_deadline() {
    let guest = Guest::start(&["sh", "-c", "trap '' TERM; exec sleep 30"], None);
    let host = guest.listen();
    assert!(matches!(host.next(), Event::Started { .. }));
    assert_eq!(host.next(), Event::Ready);
    // Give sh time to install the trap before the SIGTERM arrives.
    thread::sleep(Duration::from_millis(200));

    let mut control = guest.control();
    let started = Instant::now();
    control.request(&stop(300));
    // A repeat with a longer timeout must not push SIGKILL back.
    assert_eq!(control.state(), GuestState::Stopping);
    control.request(&stop(60_000));
    assert_eq!(
        host.next(),
        Event::Exited {
            status: ExitStatus::Signal(9)
        }
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    guest.wait();
}

#[test]
fn unknown_and_bad_requests_get_errors() {
    let guest = Guest::start(&["sleep", "30"], None);
    let host = guest.listen();
    assert!(matches!(host.next(), Event::Started { .. }));

    let stream = UnixStream::connect(guest.dir.path().join("control.sock")).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut reader = FrameReader::new(BufReader::new(stream.try_clone().unwrap()));
    let mut raw = stream.try_clone().unwrap();
    let mut send = |line: &str| -> Response {
        raw.write_all(line.as_bytes()).unwrap();
        reader.recv().unwrap().unwrap()
    };
    let code = |r: Response| match r {
        Response::Error { code, .. } => code,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(
        code(send("{\"v\":1,\"type\":\"reload\"}\n")),
        ErrorCode::Unsupported
    );
    assert_eq!(code(send("not json\n")), ErrorCode::BadRequest);
    assert_eq!(
        code(send("{\"v\":9,\"type\":\"health\"}\n")),
        ErrorCode::BadRequest
    );
    // The connection is still usable.
    assert!(matches!(
        send("{\"v\":1,\"type\":\"health\"}\n"),
        Response::Health(_)
    ));

    guest.control().request(&stop(5000));
    guest.wait();
}

#[test]
fn events_survive_a_late_and_a_restarted_host() {
    let guest = Guest::start(&["sleep", "30"], None);

    // The host comes up after the guest has already started.
    thread::sleep(Duration::from_millis(300));
    let host = guest.listen();
    assert!(matches!(host.next(), Event::Started { .. }));
    assert_eq!(host.next(), Event::Ready);

    // The host restarts; the guest keeps running and reconnects.
    host.kill();
    thread::sleep(Duration::from_millis(100));
    let host = guest.listen();
    assert_eq!(guest.control().state(), GuestState::Ready);
    guest.control().request(&stop(5000));
    assert_eq!(
        host.next(),
        Event::Exited {
            status: ExitStatus::Signal(15)
        }
    );
    guest.wait();
}
