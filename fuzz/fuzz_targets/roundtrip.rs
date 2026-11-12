//! Everything `FrameWriter::send` emits is one ADR 0004 frame, and a stream of
//! them decodes back to the same values with `FrameReader::recv`.

#![no_main]

use std::fmt::Debug;

use arbitrary::Arbitrary;
use dassie_proto::{Error, Event, FrameReader, FrameWriter, MAX_LINE, Message, Request, Response};
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Arbitrary)]
enum Input {
    Events(Vec<Event>),
    Requests(Vec<Request>),
    Responses(Vec<Response>),
}

fuzz_target!(|input: Input| match input {
    Input::Events(msgs) => roundtrip(&msgs),
    Input::Requests(msgs) => roundtrip(&msgs),
    Input::Responses(msgs) => roundtrip(&msgs),
});

fn roundtrip<T: Message + PartialEq + Debug>(msgs: &[T]) {
    let mut stream = Vec::new();
    let mut sent = Vec::new();
    for msg in msgs {
        let mut w = FrameWriter::new(Vec::new());
        let res = w.send(msg);
        let frame = w.into_inner();
        match res {
            Ok(()) => {
                assert!(frame.len() <= MAX_LINE, "sent {} bytes", frame.len());
                let Some((&b'\n', line)) = frame.split_last() else {
                    panic!("frame not newline-terminated: {frame:?}");
                };
                assert!(!line.contains(&b'\n'), "embedded newline: {frame:?}");
                assert!(line.starts_with(br#"{"v":1,"type":""#), "envelope first");
                stream.extend_from_slice(&frame);
                sent.push(msg);
            }
            // Oversized messages are refused before anything is written.
            Err(Error::LineTooLong) => assert!(frame.is_empty()),
            Err(e) => panic!("send failed: {e}"),
        }
    }

    let mut r = FrameReader::new(stream.as_slice());
    for want in sent {
        assert_eq!(r.recv::<T>().expect("recv").as_ref(), Some(want));
    }
    assert!(r.recv::<T>().expect("recv at EOF").is_none());
}
