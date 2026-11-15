//! `dassie guest-drive`: builds a VM's guest drive from its `guest.json`
//! (ADR 0015).

use std::error::Error;
use std::io::Read;
use std::path::{Path, PathBuf};

use dassie_image::{FileEntry, Options, build_files};
use dassie_proto::GuestConfig;
use dassie_proto::guest_config::{GUEST_FILE, MAX_CONFIG_BYTES};

/// Decodes `guest.json` from `input` with init's checks and writes the
/// erofs holding it to `out`. The drive holds the canonical encoding, so
/// whitespace or key order in the input never changes its bytes.
pub(crate) fn build(input: impl Read, out: &Path, mkfs: PathBuf) -> Result<(), Box<dyn Error>> {
    let mut bytes = Vec::new();
    // One byte over the cap, so decode reports an oversized input.
    input
        .take(u64::try_from(MAX_CONFIG_BYTES)? + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read guest.json: {e}"))?;
    let json = GuestConfig::decode(&bytes)?.encode()?;
    let file = FileEntry {
        name: GUEST_FILE,
        mode: 0o400,
        contents: &json,
    };
    build_files(&[file], out, &Options { mkfs })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A stand-in `mkfs.erofs` that writes its stdin, the tar, to its last
    /// argument, the output path.
    fn fake_mkfs(dir: &Path) -> PathBuf {
        let path = dir.join("mkfs.erofs");
        fs::write(&path, "#!/bin/sh\nfor out; do :; done\ncat > \"$out\"\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn writes_the_canonical_config() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("guest.drive");
        let input = br#"{ "instance_id": "web-a", "version": 1 }"#;
        build(&input[..], &out, fake_mkfs(dir.path())).unwrap();
        let tar = fs::read(&out).unwrap();
        let want = br#"{"version":1,"instance_id":"web-a"}"#;
        assert!(tar.windows(want.len()).any(|w| w == want));
    }

    #[test]
    fn refuses_what_init_would() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("guest.drive");
        for input in [
            &br#"{"version":1,"instance_id":"web-a","future":1}"#[..],
            br#"{"version":2,"instance_id":"web-a"}"#,
            br#"{"version":1,"instance_id":"127.0.0.1"}"#,
        ] {
            assert!(build(input, &out, fake_mkfs(dir.path())).is_err());
            assert!(!out.exists());
        }
    }
}
