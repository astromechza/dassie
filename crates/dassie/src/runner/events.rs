//! The events channel's host end: guest connections on `<uds_path>_1024`
//! (ADR 0004).

use std::io::BufReader;
use std::os::unix::net::UnixListener;
use std::sync::mpsc::SyncSender;
use std::thread;

use dassie_proto::{Error, Event, FrameReader};

use super::Msg;

/// Serves guest connections one at a time, forwarding each event, until the
/// receiver is gone. The guest holds one connection at a time, reconnecting
/// only after a failed write. `tx` is bounded: while the main loop is behind,
/// this thread stops reading, and the guest's writes wait.
///
/// `exited` is the last event of a boot. Once it is forwarded, the
/// connection is closed: that close tells the guest it may reboot
/// (ADR 0018).
pub(crate) fn spawn(listener: UnixListener, tx: SyncSender<Msg>) {
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
                        let exited = matches!(event, Event::Exited { .. });
                        if tx.send(Msg::Event(event)).is_err() {
                            return;
                        }
                        if exited {
                            break;
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
            // Close before reporting, which can wait behind a full queue.
            drop(reader);
            if tx.send(Msg::Disconnected).is_err() {
                return;
            }
        }
    });
}
