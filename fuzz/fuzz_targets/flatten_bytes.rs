//! Feeds arbitrary bytes to the flattener as a single layer. Any input may be
//! rejected, but nothing may panic, and an accepted layer must flatten to a
//! well-formed stream (parents first, no duplicates, hard links after their
//! targets, `/.dassie/image.json` last) and the same bytes every time.

#![no_main]

use dassie_fuzz::{check_output, flatten};
use libfuzzer_sys::fuzz_target;

const CONFIG: &[u8] = br#"{"config":{"Cmd":["/app"]}}"#;

fuzz_target!(|data: &[u8]| {
    match flatten(&[data], CONFIG) {
        Err(e) => {
            let _ = e.to_string();
        }
        Ok(out) => {
            check_output(&out, CONFIG);
            assert!(
                flatten(&[data], CONFIG).ok() == Some(out),
                "second run differs"
            );
        }
    }
});
