//! Newline-delimited JSON framing, generic over `Read`/`Write`.
//!
//! Each frame is one compact JSON object terminated by `\n`, at most
//! [`MAX_LINE`] bytes including the terminator, carrying `"v"` and `"type"`
//! envelope fields first (ADR 0004).

use std::fmt;
use std::io::{self, BufRead, Read, Write};

use serde::{Deserialize, Serialize};

use crate::message::Message;
use crate::{MAX_LINE, PROTOCOL_VERSION};

/// Errors from sending or receiving a frame.
///
/// After [`Error::UnsupportedVersion`], [`Error::UnknownType`] or [`Error::Invalid`] the
/// reader is positioned at the next frame and can keep going. After
/// [`Error::Io`] or [`Error::LineTooLong`] the connection should be closed.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The frame exceeds [`MAX_LINE`] bytes.
    LineTooLong,
    /// The frame carries a protocol version this build does not speak.
    UnsupportedVersion(u64),
    /// The frame has a `type` this message enum does not know.
    UnknownType(String),
    /// The frame is not valid JSON or does not match its type's fields.
    Invalid(serde_json::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "i/o error: {e}"),
            Self::LineTooLong => write!(f, "frame exceeds {MAX_LINE} bytes"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
            Self::UnknownType(t) => write!(f, "unknown message type {t:?}"),
            Self::Invalid(e) => write!(f, "invalid message: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Invalid(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Serialize)]
struct Envelope<'a, T> {
    v: u32,
    #[serde(flatten)]
    msg: &'a T,
}

#[derive(Deserialize)]
struct Header {
    v: u64,
    #[serde(rename = "type")]
    ty: String,
}

/// Writes frames to `W`, flushing after each one.
#[derive(Debug)]
pub struct FrameWriter<W> {
    inner: W,
}

impl<W: Write> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    pub fn send<T: Message>(&mut self, msg: &T) -> Result<(), Error> {
        let mut buf = serde_json::to_vec(&Envelope {
            v: PROTOCOL_VERSION,
            msg,
        })
        .map_err(Error::Invalid)?;
        buf.push(b'\n');
        if buf.len() > MAX_LINE {
            return Err(Error::LineTooLong);
        }
        self.inner.write_all(&buf)?;
        self.inner.flush()?;
        Ok(())
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

/// Reads frames from `R`.
#[derive(Debug)]
pub struct FrameReader<R> {
    inner: R,
    buf: Vec<u8>,
}

impl<R: BufRead> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }

    /// Reads the next frame. Returns `Ok(None)` on a clean EOF between frames.
    pub fn recv<T: Message>(&mut self) -> Result<Option<T>, Error> {
        self.buf.clear();
        let n = Read::by_ref(&mut self.inner)
            .take(MAX_LINE as u64)
            .read_until(b'\n', &mut self.buf)?;
        if n == 0 {
            return Ok(None);
        }
        let Some((&b'\n', line)) = self.buf.split_last() else {
            return Err(if n == MAX_LINE {
                Error::LineTooLong
            } else {
                Error::Io(io::ErrorKind::UnexpectedEof.into())
            });
        };
        let header: Header = serde_json::from_slice(line).map_err(Error::Invalid)?;
        if header.v != u64::from(PROTOCOL_VERSION) {
            return Err(Error::UnsupportedVersion(header.v));
        }
        if !T::TYPES.contains(&header.ty.as_str()) {
            return Err(Error::UnknownType(header.ty));
        }
        serde_json::from_slice(line)
            .map(Some)
            .map_err(Error::Invalid)
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::message::*;

    fn encode<T: Message>(msg: &T) -> String {
        let mut w = FrameWriter::new(Vec::new());
        w.send(msg).unwrap();
        String::from_utf8(w.into_inner()).unwrap()
    }

    fn decode<T: Message>(line: &str) -> Result<Option<T>, Error> {
        FrameReader::new(Cursor::new(line.as_bytes())).recv()
    }

    fn check<T: Message + PartialEq + fmt::Debug>(cases: &[(T, &str)]) {
        for (msg, line) in cases {
            assert_eq!(encode(msg), format!("{line}\n"));
            assert_eq!(
                decode::<T>(&format!("{line}\n")).unwrap().as_ref(),
                Some(msg)
            );
        }
    }

    /// The exact lines from ADR 0004; these guard the wire contract.
    #[test]
    fn wire_shapes() {
        let events = [
            (
                Event::Started { pid: 42 },
                r#"{"v":1,"type":"started","pid":42}"#,
            ),
            (Event::Ready, r#"{"v":1,"type":"ready"}"#),
            (
                Event::Exited {
                    status: ExitStatus::Code(3),
                },
                r#"{"v":1,"type":"exited","code":3}"#,
            ),
            (
                Event::Exited {
                    status: ExitStatus::Signal(9),
                },
                r#"{"v":1,"type":"exited","signal":9}"#,
            ),
        ];
        let requests = [
            (Request::Health, r#"{"v":1,"type":"health"}"#),
            (
                Request::Stop {
                    timeout_ms: Some(5000),
                },
                r#"{"v":1,"type":"stop","timeout_ms":5000}"#,
            ),
            (
                Request::Stop { timeout_ms: None },
                r#"{"v":1,"type":"stop"}"#,
            ),
        ];
        let responses = [
            (
                Response::Health(HealthStatus {
                    state: GuestState::Ready,
                    pid: 42,
                }),
                r#"{"v":1,"type":"health","state":"ready","pid":42}"#,
            ),
            (
                Response::Health(HealthStatus {
                    state: GuestState::Stopping,
                    pid: 42,
                }),
                r#"{"v":1,"type":"health","state":"stopping","pid":42}"#,
            ),
            (
                Response::Error {
                    code: ErrorCode::Unsupported,
                    message: "unknown request type \"reload\"".into(),
                },
                r#"{"v":1,"type":"error","code":"unsupported","message":"unknown request type \"reload\""}"#,
            ),
        ];

        check(&events);
        check(&requests);
        check(&responses);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let got: Option<Event> =
            decode("{\"v\":1,\"type\":\"started\",\"pid\":7,\"extra\":true}\n").unwrap();
        assert_eq!(got, Some(Event::Started { pid: 7 }));
        let got: Option<Event> = decode("{\"v\":1,\"type\":\"ready\",\"extra\":[1]}\n").unwrap();
        assert_eq!(got, Some(Event::Ready));
    }

    #[test]
    fn exited_needs_exactly_one_status() {
        for line in [
            "{\"v\":1,\"type\":\"exited\"}\n",
            "{\"v\":1,\"type\":\"exited\",\"code\":1,\"signal\":9}\n",
        ] {
            assert!(
                matches!(decode::<Event>(line), Err(Error::Invalid(_))),
                "{line}"
            );
        }
    }

    #[test]
    fn rejects_other_versions_and_missing_envelope() {
        assert!(matches!(
            decode::<Event>("{\"v\":2,\"type\":\"ready\"}\n"),
            Err(Error::UnsupportedVersion(2))
        ));
        assert!(matches!(
            decode::<Event>("{\"type\":\"ready\"}\n"),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            decode::<Event>("{\"v\":1}\n"),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn unknown_type_is_distinct_and_skippable() {
        let input = "{\"v\":1,\"type\":\"reload\"}\n{\"v\":1,\"type\":\"health\"}\n";
        let mut r = FrameReader::new(Cursor::new(input.as_bytes()));
        assert!(matches!(r.recv::<Request>(), Err(Error::UnknownType(t)) if t == "reload"));
        assert_eq!(r.recv::<Request>().unwrap(), Some(Request::Health));
        assert_eq!(r.recv::<Request>().unwrap(), None);
    }

    #[test]
    fn invalid_frame_is_skippable() {
        let input = "not json\n{\"v\":1,\"type\":\"ready\"}\n";
        let mut r = FrameReader::new(Cursor::new(input.as_bytes()));
        assert!(matches!(r.recv::<Event>(), Err(Error::Invalid(_))));
        assert_eq!(r.recv::<Event>().unwrap(), Some(Event::Ready));
    }

    #[test]
    fn blank_line_is_invalid() {
        assert!(matches!(decode::<Event>("\n"), Err(Error::Invalid(_))));
    }

    #[test]
    fn line_too_long() {
        let mut line = format!(
            "{{\"v\":1,\"type\":\"ready\",\"pad\":\"{}",
            "x".repeat(MAX_LINE)
        );
        line.push_str("\"}\n");
        assert!(matches!(decode::<Event>(&line), Err(Error::LineTooLong)));

        // Exactly MAX_LINE bytes including the terminator is allowed.
        let prefix = "{\"v\":1,\"type\":\"ready\",\"pad\":\"";
        let pad = MAX_LINE - prefix.len() - "\"}\n".len();
        let line = format!("{prefix}{}\"}}\n", "x".repeat(pad));
        assert_eq!(line.len(), MAX_LINE);
        assert_eq!(decode::<Event>(&line).unwrap(), Some(Event::Ready));
    }

    #[test]
    fn eof_mid_frame_is_an_error() {
        assert!(matches!(
            decode::<Event>("{\"v\":1,\"type\":\"ready\"}"),
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
    }

    #[test]
    fn many_frames_over_a_socket_pair() {
        use std::io::BufReader;
        use std::os::unix::net::UnixStream;

        let (a, b) = UnixStream::pair().unwrap();
        let sent = vec![
            Event::Started { pid: 1 },
            Event::Ready,
            Event::Exited {
                status: ExitStatus::Code(0),
            },
        ];
        let writer = {
            let sent = sent.clone();
            std::thread::spawn(move || {
                let mut w = FrameWriter::new(a);
                for e in &sent {
                    w.send(e).unwrap();
                }
            })
        };
        let mut r = FrameReader::new(BufReader::new(b));
        let mut got = Vec::new();
        while let Some(e) = r.recv::<Event>().unwrap() {
            got.push(e);
        }
        writer.join().unwrap();
        assert_eq!(got, sent);
    }
}
