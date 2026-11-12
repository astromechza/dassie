//! Boot parameters the PID-1 layer reads from the kernel cmdline before it
//! switches into the app rootfs (ADR 0006).

use crate::config::split_cmdline;

/// The app rootfs drive to mount and switch into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSpec {
    /// Block device path, from `root=`. Defaults to the first virtio-blk
    /// drive, `/dev/vda`.
    pub device: String,
    /// Filesystem type, from `rootfstype=`. Defaults to `ext4`.
    pub fstype: String,
    /// Mount read-only: `ro`, unless a later `rw` overrides it. Defaults to
    /// read-write, unlike the kernel, because the app rootfs is per-VM.
    pub read_only: bool,
}

impl RootSpec {
    /// Parses `root=`, `rootfstype=`, `ro` and `rw` the way the kernel does:
    /// the last occurrence wins. Empty values fall back to the defaults.
    pub fn from_cmdline(cmdline: &str) -> Self {
        let mut spec = Self {
            device: "/dev/vda".to_owned(),
            fstype: "ext4".to_owned(),
            read_only: false,
        };
        for token in split_cmdline(cmdline) {
            match token.split_once('=') {
                Some(("root", dev)) if !dev.is_empty() => dev.clone_into(&mut spec.device),
                Some(("rootfstype", fs)) if !fs.is_empty() => fs.clone_into(&mut spec.fstype),
                None if token == "ro" => spec.read_only = true,
                None if token == "rw" => spec.read_only = false,
                _ => {}
            }
        }
        spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_the_first_virtio_drive() {
        assert_eq!(
            RootSpec::from_cmdline("console=ttyS0 reboot=k panic=1"),
            RootSpec {
                device: "/dev/vda".into(),
                fstype: "ext4".into(),
                read_only: false,
            }
        );
    }

    #[test]
    fn last_occurrence_wins() {
        assert_eq!(
            RootSpec::from_cmdline("root=/dev/vda ro rootfstype=xfs root=/dev/vdb rw ro"),
            RootSpec {
                device: "/dev/vdb".into(),
                fstype: "xfs".into(),
                read_only: true,
            }
        );
    }

    #[test]
    fn ignores_lookalikes_and_empty_values() {
        let spec = RootSpec::from_cmdline("root= rootfstype= dassie.root=/dev/x rootwait ro=1");
        assert_eq!(spec, RootSpec::from_cmdline(""));
    }
}
