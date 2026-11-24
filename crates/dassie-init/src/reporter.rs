//! Guest side of the events channel: best-effort delivery with reconnect
//! (ADR 0004), and the wait for the host to acknowledge `exited` by closing
//! the connection (ADR 0018).

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use dassie_proto::{Event, FrameWriter};

/// An events connection. The guest only reads from it to see the host close
/// it.
pub trait Conn: Read + Write + Send {}

impl<T: Read + Write + Send> Conn for T {}

/// Opens a new events connection: vsock to the host in the guest, a Unix
/// socket in tests.
pub type Connector = Box<dyn FnMut() -> io::Result<Box<dyn Conn>> + Send>;

const RETRY_INTERVAL: Duration = Duration::from_millis(500);

enum Msg {
    Event(Event),
    Flush(mpsc::Sender<()>),
    Finish(mpsc::Sender<()>),
}

/// Handle to the reporter thread. Dropping it stops the thread once it is
/// idle.
#[derive(Debug)]
pub struct Reporter {
    tx: mpsc::Sender<Msg>,
}

impl Reporter {
    pub fn start(connect: Connector) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || run(connect, &rx));
        Self { tx }
    }

    /// Queues an event; never blocks.
    pub fn send(&self, event: Event) {
        // The thread only exits once this handle is dropped.
        drop(self.tx.send(Msg::Event(event)));
    }

    /// Waits up to `timeout` for every queued event to be written. Returns
    /// whether that happened.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (ack_tx, ack_rx) = mpsc::channel();
        self.tx.send(Msg::Flush(ack_tx)).is_ok() && ack_rx.recv_timeout(timeout).is_ok()
    }

    /// Waits up to `timeout` for every queued event to be written and then
    /// for the host to close the connection, which it does once it has read
    /// `exited`. Returns whether that happened.
    ///
    /// A written event has only reached the guest kernel: it can still be
    /// lost if the VM stops before Firecracker forwards it, so only the
    /// host's close shows that `exited` arrived.
    pub fn finish(self, timeout: Duration) -> bool {
        let (ack_tx, ack_rx) = mpsc::channel();
        self.tx.send(Msg::Finish(ack_tx)).is_ok() && ack_rx.recv_timeout(timeout).is_ok()
    }
}

fn run(mut connect: Connector, rx: &mpsc::Receiver<Msg>) {
    let mut pending: Vec<Event> = Vec::new();
    let mut flushers: Vec<mpsc::Sender<()>> = Vec::new();
    let mut finisher: Option<mpsc::Sender<()>> = None;
    let mut conn: Option<FrameWriter<Box<dyn Conn>>> = None;
    loop {
        deliver(&mut pending, &mut conn, &mut connect);
        if pending.is_empty() {
            for ack in flushers.drain(..) {
                let _ = ack.send(());
            }
            if let Some(ack) = finisher.take() {
                if let Some(writer) = conn.take() {
                    wait_for_close(writer.into_inner());
                }
                let _ = ack.send(());
                return;
            }
        }
        let msg = if pending.is_empty() {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            rx.recv_timeout(RETRY_INTERVAL)
        };
        match msg {
            Ok(Msg::Event(event)) => pending.push(event),
            Ok(Msg::Flush(ack)) => flushers.push(ack),
            Ok(Msg::Finish(ack)) => finisher = Some(ack),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Writes pending events in order. A failed connect or write keeps the event
/// pending and drops the connection, so the next attempt reconnects.
fn deliver(
    pending: &mut Vec<Event>,
    conn: &mut Option<FrameWriter<Box<dyn Conn>>>,
    connect: &mut Connector,
) {
    while let Some(event) = pending.first() {
        if conn.is_none() {
            match connect() {
                Ok(stream) => *conn = Some(FrameWriter::new(stream)),
                Err(_) => return,
            }
        }
        if let Some(writer) = conn {
            if writer.send(event).is_err() {
                *conn = None;
                return;
            }
            pending.remove(0);
        }
    }
}

/// Reads until the host closes the connection. The host sends nothing, so
/// anything read is ignored. An error, such as a reset, also means it is
/// closed.
fn wait_for_close(mut conn: Box<dyn Conn>) {
    let mut buf = [0; 64];
    loop {
        match conn.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A connector whose connections append to a shared log, and which fails
    /// while `up` is false or after `writes_left` writes. Reads block until
    /// the host has `closed` the connection.
    #[derive(Clone, Default)]
    struct Fake {
        up: Arc<Mutex<bool>>,
        writes_left: Arc<Mutex<Option<usize>>>,
        log: Arc<Mutex<Vec<u8>>>,
        closed: Arc<AtomicBool>,
    }

    struct FakeConn(Fake);

    impl Write for FakeConn {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut left = self.0.writes_left.lock().unwrap();
            match left.as_mut() {
                Some(0) => return Err(io::ErrorKind::BrokenPipe.into()),
                Some(n) => *n -= 1,
                None => {}
            }
            self.0.log.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Read for FakeConn {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            while !self.0.closed.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
            Ok(0)
        }
    }

    impl Fake {
        fn connector(&self) -> Connector {
            let fake = self.clone();
            Box::new(move || {
                if *fake.up.lock().unwrap() {
                    Ok(Box::new(FakeConn(fake.clone())) as Box<dyn Conn>)
                } else {
                    Err(io::ErrorKind::ConnectionRefused.into())
                }
            })
        }

        fn lines(&self) -> Vec<String> {
            let log = self.log.lock().unwrap();
            String::from_utf8_lossy(&log)
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    #[test]
    fn holds_events_until_the_host_listens() {
        let fake = Fake::default();
        let reporter = Reporter::start(fake.connector());
        reporter.send(Event::Started { pid: 1 });
        reporter.send(Event::Ready);
        assert!(!reporter.flush(Duration::from_millis(100)));
        assert_eq!(fake.lines(), Vec::<String>::new());

        *fake.up.lock().unwrap() = true;
        assert!(reporter.flush(Duration::from_secs(2)));
        assert_eq!(
            fake.lines(),
            [
                r#"{"v":1,"type":"started","pid":1}"#,
                r#"{"v":1,"type":"ready"}"#
            ]
        );
    }

    #[test]
    fn retries_an_event_after_a_failed_write() {
        let fake = Fake::default();
        *fake.up.lock().unwrap() = true;
        let reporter = Reporter::start(fake.connector());
        reporter.send(Event::Started { pid: 1 });
        assert!(reporter.flush(Duration::from_secs(1)));

        // The connection breaks: the next write fails, then the host is back.
        *fake.writes_left.lock().unwrap() = Some(0);
        reporter.send(Event::Ready);
        thread::sleep(Duration::from_millis(50));
        *fake.writes_left.lock().unwrap() = None;
        assert!(reporter.flush(Duration::from_secs(2)));
        assert_eq!(
            fake.lines(),
            [
                r#"{"v":1,"type":"started","pid":1}"#,
                r#"{"v":1,"type":"ready"}"#
            ]
        );
    }

    const EXITED: &str = r#"{"v":1,"type":"exited","code":3}"#;

    fn exited() -> Event {
        Event::Exited {
            status: dassie_proto::ExitStatus::Code(3),
        }
    }

    #[test]
    fn finish_waits_for_the_host_to_close_after_exited() {
        // `exited` is written, as far as the guest can tell, but the host
        // never confirms it: it may still be in the guest kernel or in
        // Firecracker when the VM stops, so finishing must not report it
        // delivered.
        let fake = Fake::default();
        *fake.up.lock().unwrap() = true;
        let reporter = Reporter::start(fake.connector());
        reporter.send(exited());
        assert!(reporter.flush(Duration::from_secs(1)));
        assert_eq!(fake.lines(), [EXITED]);
        assert!(!reporter.finish(Duration::from_millis(200)));
    }

    #[test]
    fn finish_returns_once_the_host_has_read_exited_and_closed() {
        let fake = Fake::default();
        *fake.up.lock().unwrap() = true;
        let reporter = Reporter::start(fake.connector());
        reporter.send(Event::Started { pid: 1 });
        reporter.send(exited());
        // The host closes once it has read `exited`.
        let host = fake.clone();
        thread::spawn(move || {
            while !host.lines().iter().any(|l| l == EXITED) {
                thread::sleep(Duration::from_millis(1));
            }
            host.closed.store(true, Ordering::SeqCst);
        });
        assert!(reporter.finish(Duration::from_secs(2)));
        assert!(fake.closed.load(Ordering::SeqCst));
    }

    #[test]
    fn finish_still_delivers_exited_to_a_host_that_comes_up_late() {
        let fake = Fake::default();
        let reporter = Reporter::start(fake.connector());
        reporter.send(exited());
        let host = fake.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            *host.up.lock().unwrap() = true;
            while host.lines().is_empty() {
                thread::sleep(Duration::from_millis(1));
            }
            host.closed.store(true, Ordering::SeqCst);
        });
        assert!(reporter.finish(Duration::from_secs(2)));
        assert_eq!(fake.lines(), [EXITED]);
    }
}
