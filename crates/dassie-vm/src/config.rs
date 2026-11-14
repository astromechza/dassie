//! Reading Firecracker `--config-file` JSON.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The parts of a Firecracker config file dassie reads. Everything else is
/// ignored, so the file stays Firecracker's format.
#[derive(Deserialize)]
struct Config {
    vsock: Option<Vsock>,
}

#[derive(Deserialize)]
struct Vsock {
    uds_path: PathBuf,
}

/// Returns the vsock device's `uds_path` from a Firecracker config file.
///
/// Fails if the file cannot be read, is not JSON, or has no vsock device.
pub fn vsock_uds_path(config: &Path) -> io::Result<PathBuf> {
    let bytes = fs::read(config)
        .map_err(|e| io::Error::new(e.kind(), format!("read {}: {e}", config.display())))?;
    let parsed: Config = serde_json::from_slice(&bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("parse {}: {e}", config.display()),
        )
    })?;
    parsed.vsock.map(|v| v.uds_path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: no vsock device", config.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> io::Result<PathBuf> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, json).unwrap();
        vsock_uds_path(&path)
    }

    #[test]
    fn reads_the_vsock_uds_path() {
        let json = r#"{
            "boot-source": { "kernel_image_path": "/k" },
            "vsock": { "guest_cid": 3, "uds_path": "/run/dassie/a.vsock" }
        }"#;
        assert_eq!(parse(json).unwrap(), Path::new("/run/dassie/a.vsock"));
    }

    #[test]
    fn rejects_a_config_without_vsock() {
        let err = parse(r#"{ "boot-source": {} }"#).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("no vsock device"), "{err}");
    }

    #[test]
    fn rejects_invalid_json() {
        let err = parse("{").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
