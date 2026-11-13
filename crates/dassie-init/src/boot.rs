//! Boot parameters the PID-1 layer reads from the kernel cmdline before it
//! switches into the app rootfs (ADR 0006).

use crate::config::split_cmdline;

/// The app rootfs drive to mount and switch into.
///
/// The drive is always mounted read-only, as the lower layer of an overlay
/// whose upper layer is a tmpfs, so `ro` and `rw` on the cmdline change
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSpec {
    /// Block device path, from `root=`. Defaults to the first virtio-blk
    /// drive, `/dev/vda`.
    pub device: String,
    /// Filesystem type, from `rootfstype=`. Defaults to `erofs`, the format
    /// of image artefacts (ADR 0009).
    pub fstype: String,
}

impl RootSpec {
    /// Parses `root=` and `rootfstype=` the way the kernel does: the last
    /// occurrence wins. Empty values fall back to the defaults.
    pub fn from_cmdline(cmdline: &str) -> Self {
        let mut spec = Self {
            device: "/dev/vda".to_owned(),
            fstype: "erofs".to_owned(),
        };
        for token in split_cmdline(cmdline) {
            match token.split_once('=') {
                Some(("root", dev)) if !dev.is_empty() => dev.clone_into(&mut spec.device),
                Some(("rootfstype", fs)) if !fs.is_empty() => fs.clone_into(&mut spec.fstype),
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
            RootSpec::from_cmdline("console=ttyS0 reboot=k panic=1 ro"),
            RootSpec {
                device: "/dev/vda".into(),
                fstype: "erofs".into(),
            }
        );
    }

    #[test]
    fn last_occurrence_wins() {
        assert_eq!(
            RootSpec::from_cmdline("root=/dev/vda rootfstype=xfs root=/dev/vdb rootfstype=ext4"),
            RootSpec {
                device: "/dev/vdb".into(),
                fstype: "ext4".into(),
            }
        );
    }

    #[test]
    fn ignores_lookalikes_and_empty_values() {
        let spec = RootSpec::from_cmdline("root= rootfstype= dassie.root=/dev/x rootwait rw");
        assert_eq!(spec, RootSpec::from_cmdline(""));
    }
}
