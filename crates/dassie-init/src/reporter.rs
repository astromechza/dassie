//! Guest side of the events channel: best-effort delivery with reconnect
//! (ADR 0004).

use std::io::{self, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use dassie_proto::{Event, FrameWriter};

/// Opens a new events connection: vsock to the host in the guest, a Unix
/// socket in tests.
pub type Connector = Box<dyn FnMut() -> io::Result<Box<dyn Write + Send>> + Send>;

const RETRY_INTERVAL: Duration = Duration::from_millis(500);

enum Msg {
    Event(Event),
    Flush(mpsc::Sender<()>),
}

/// Handle to the reporter thread. Dropping it stops the thread once it is
/// idle.
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
        let _ = self.tx.send(Msg::Event(event));
    }

    /// Waits up to `timeout` for every queued event to be written. Returns
    /// whether that happened.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (ack_tx, ack_rx) = mpsc::channel();
        self.tx.send(Msg::Flush(ack_tx)).is_ok() && ack_rx.recv_timeout(timeout).is_ok()
    }
}

fn run(mut connect: Connector, rx: &mpsc::Receiver<Msg>) {
    let mut pending: Vec<Event> = Vec::new();
    let mut flushers: Vec<mpsc::Sender<()>> = Vec::new();
    let mut conn: Option<FrameWriter<Box<dyn Write + Send>>> = None;
    loop {
        deliver(&mut pending, &mut conn, &mut connect);
        if pending.is_empty() {
            for ack in flushers.drain(..) {
                let _ = ack.send(());
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
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Writes pending events in order. A failed connect or write keeps the event
/// pending and drops the connection, so the next attempt reconnects.
fn deliver(
    pending: &mut Vec<Event>,
    conn: &mut Option<FrameWriter<Box<dyn Write + Send>>>,
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A connector whose connections append to a shared log, and which fails
    /// while `up` is false or after `writes_left` writes.
    #[derive(Clone, Default)]
    struct Fake {
        up: Arc<Mutex<bool>>,
        writes_left: Arc<Mutex<Option<usize>>>,
        log: Arc<Mutex<Vec<u8>>>,
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

    impl Fake {
        fn connector(&self) -> Connector {
            let fake = self.clone();
            Box::new(move || {
                if *fake.up.lock().unwrap() {
                    Ok(Box::new(FakeConn(fake.clone())) as Box<dyn Write + Send>)
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
        assert!(fake.lines().is_empty());

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
}
