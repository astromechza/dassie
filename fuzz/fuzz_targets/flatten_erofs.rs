//! Like `flatten_bytes`, but an accepted layer's flattened stream is also
//! built with the real `mkfs.erofs --tar=f` and loop-mounted, and the
//! filesystem must hold the tree the `tar` crate reads
//! from that stream: same paths, types, contents, link targets, hard link
//! groups, modes, owners, mtimes, device numbers and xattrs (see
//! `src/erofs.rs`).
//!
//! It spawns two processes per input, so it is slow to fuzz with. CI
//! replays the corpus `flatten_bytes` grew through it instead (`-runs=0`).
//! Linux only, as root, with the erofs module and erofs-utils on `PATH`.

#![no_main]

use dassie_fuzz::{check_output, flatten};
use libfuzzer_sys::fuzz_target;

const CONFIG: &[u8] = br#"{"config":{"Cmd":["/app"]}}"#;

fuzz_target!(|data: &[u8]| {
    if let Ok(out) = flatten(&[data], CONFIG) {
        check_output(&out, CONFIG);
        #[cfg(target_os = "linux")]
        dassie_fuzz::erofs::check_erofs(&out);
    }
});
