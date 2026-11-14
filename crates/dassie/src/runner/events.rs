//! The events channel's host end: guest connections on `<uds_path>_1024`
//! (ADR 0004).

use std::io::BufReader;
use std::os::unix::net::UnixListener;
use std::sync::mpsc::Sender;
use std::thread;

use dassie_proto::{Error, Event, FrameReader};

use super::Msg;

/// Serves guest connections one at a time, forwarding each event, until the
/// receiver is gone. The guest holds one connection at a time, reconnecting
/// only after a failed write.
pub(crate) fn spawn(listener: UnixListener, tx: Sender<Msg>) {
    thread::spawn(move || {
        for conn in listener.incoming() {
            let conn = match conn {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("vm-run: events accept: {e}");
                    continue;
                }
            };
            if tx.send(Msg::Connected).is_err() {
                return;
            }
            let mut reader = FrameReader::new(BufReader::new(conn));
            loop {
                match reader.recv::<Event>() {
                    Ok(Some(event)) => {
                        if tx.send(Msg::Event(event)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    // Skippable: the reader is at the next frame.
                    Err(
                        e @ (Error::UnsupportedVersion(_)
                        | Error::UnknownType(_)
                        | Error::Invalid(_)),
                    ) => eprintln!("vm-run: skipping event frame: {e}"),
                    Err(e) => {
                        eprintln!("vm-run: events connection: {e}");
                        break;
                    }
                }
            }
            if tx.send(Msg::Disconnected).is_err() {
                return;
            }
        }
    });
}
