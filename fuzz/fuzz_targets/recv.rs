//! Decodes arbitrary bytes as a stream of `Event`, `Request` and `Response`
//! frames and checks every `FrameReader::recv` outcome against ADR 0004:
//! nothing panics, every call makes progress, fatal errors happen only where
//! the ADR says, and every accepted frame is one the writer would produce.

#![no_main]

use std::fmt::Debug;
use std::io::{Cursor, ErrorKind};

use dassie_proto::{
    Error, Event, FrameReader, FrameWriter, MAX_LINE, Message, PROTOCOL_VERSION, Request, Response,
};
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    drain::<Event>(data);
    drain::<Request>(data);
    drain::<Response>(data);
});

fn drain<T: Message + PartialEq + Debug>(data: &[u8]) {
    let mut r = FrameReader::new(Cursor::new(data));
    loop {
        let start = pos(&r);
        let res = r.recv::<T>();
        let end = pos(&r);
        let consumed = &data[start..end];
        if let Err(e) = &res {
            let _ = e.to_string();
        }

        match res {
            Ok(None) => {
                assert_eq!(end, data.len(), "clean EOF before the end of input");
                return;
            }
            // Fatal: the caller closes the connection.
            Err(Error::Io(e)) => {
                assert_eq!(e.kind(), ErrorKind::UnexpectedEof, "{e}");
                assert_eq!(end, data.len(), "EOF error before the end of input");
                assert!(!consumed.is_empty() && !consumed.contains(&b'\n'));
                return;
            }
            Err(Error::LineTooLong) => {
                assert_eq!(consumed.len(), MAX_LINE);
                assert!(!consumed.contains(&b'\n'));
                return;
            }
            // Skippable: the reader is positioned at the next frame.
            Err(Error::UnsupportedVersion(v)) => {
                assert_ne!(v, u64::from(PROTOCOL_VERSION));
            }
            Err(Error::UnknownType(ty)) => {
                assert!(!T::TYPES.contains(&ty.as_str()), "{ty:?} is known");
            }
            Err(Error::Invalid(_)) => {}
            Ok(Some(msg)) => check_accepted(consumed, &msg),
        }
        let Some((&b'\n', line)) = consumed.split_last() else {
            panic!("skippable result did not consume exactly one frame: {consumed:?}");
        };
        assert!(line.len() < MAX_LINE && !line.contains(&b'\n'));
    }
}

/// An accepted frame is a JSON object whose envelope matches the decoded
/// value, and the value survives a send/recv round trip.
fn check_accepted<T: Message + PartialEq + Debug>(frame: &[u8], msg: &T) {
    let mut w = FrameWriter::new(Vec::new());
    w.send(msg).expect("an accepted message must be sendable");
    let encoded = w.into_inner();

    let got: Value = serde_json::from_slice(frame).expect("accepted frame is JSON");
    let want: Value = serde_json::from_slice(&encoded).expect("sent frame is JSON");
    let (Some(got), Some(want)) = (got.as_object(), want.as_object()) else {
        panic!("accepted frame is not a JSON object: {frame:?}");
    };
    assert_eq!(got.get("v"), want.get("v"), "envelope version");
    assert_eq!(got.get("type"), want.get("type"), "envelope type");

    let back = FrameReader::new(encoded.as_slice()).recv::<T>();
    assert_eq!(back.ok().flatten().as_ref(), Some(msg), "round trip");
}

fn pos(r: &FrameReader<Cursor<&[u8]>>) -> usize {
    usize::try_from(r.get_ref().position()).expect("cursor position fits usize")
}
