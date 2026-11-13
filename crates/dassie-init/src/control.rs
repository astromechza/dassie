//! Guest side of the control channel: answers host requests (ADR 0004).

use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use dassie_proto::{Error, ErrorCode, FrameReader, FrameWriter, HealthStatus, Request, Response};

use crate::supervisor::Control;

/// Accepts control connections: a vsock listener in the guest, a Unix socket
/// in tests. Each connection is split into a read half and a write half.
pub trait ControlListener: Send + 'static {
    fn accept(&self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)>;
}

impl ControlListener for UnixListener {
    fn accept(&self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let (stream, _) = UnixListener::accept(self)?;
        Ok((Box::new(stream.try_clone()?), Box::new(stream)))
    }
}

/// How long a `stop` waits for the supervisor to acknowledge before answering
/// from the shared status.
const STOP_ACK_TIMEOUT: Duration = Duration::from_secs(1);

/// Serves control connections on a background thread, one thread per
/// connection.
pub(crate) fn serve(
    listener: Box<dyn ControlListener>,
    status: Arc<Mutex<HealthStatus>>,
    control: mpsc::Sender<Control>,
) {
    thread::spawn(move || -> ! {
        loop {
            match listener.accept() {
                Ok((reader, writer)) => {
                    let status = Arc::clone(&status);
                    let control = control.clone();
                    thread::spawn(move || handle(reader, writer, &status, &control));
                }
                Err(e) => {
                    eprintln!("dassie-init: control accept: {e}");
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });
}

fn handle(
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    status: &Mutex<HealthStatus>,
    control: &mpsc::Sender<Control>,
) {
    let current = || *status.lock().unwrap_or_else(PoisonError::into_inner);
    let mut reader = FrameReader::new(BufReader::new(reader));
    let mut writer = FrameWriter::new(writer);
    loop {
        let response = match reader.recv::<Request>() {
            Ok(Some(Request::Health)) => Response::Health(current()),
            Ok(Some(Request::Stop { timeout_ms })) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                // If the supervisor is gone, answer from the shared status.
                drop(control.send(Control::Stop {
                    timeout: timeout_ms.map(Duration::from_millis),
                    reply: Some(reply_tx),
                }));
                Response::Health(
                    reply_rx
                        .recv_timeout(STOP_ACK_TIMEOUT)
                        .unwrap_or_else(|_| current()),
                )
            }
            Ok(Some(other)) => error(
                ErrorCode::Unsupported,
                format!("unsupported request {other:?}"),
            ),
            Err(Error::UnknownType(ty)) => error(
                ErrorCode::Unsupported,
                format!("unknown request type {ty:?}"),
            ),
            Err(e @ (Error::UnsupportedVersion(_) | Error::Invalid(_))) => {
                error(ErrorCode::BadRequest, e.to_string())
            }
            // EOF, or a framing error that leaves the stream unusable.
            Ok(None) | Err(_) => return,
        };
        if writer.send(&response).is_err() {
            return;
        }
    }
}

/// Longest `message` in an error response, in bytes. Messages can echo
/// request text, so this keeps the response well within `MAX_LINE`.
const MAX_ERROR_MESSAGE: usize = 1024;

fn error(code: ErrorCode, mut message: String) -> Response {
    if message.len() > MAX_ERROR_MESSAGE {
        let mut end = MAX_ERROR_MESSAGE;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push_str("...");
    }
    Response::Error { code, message }
}
