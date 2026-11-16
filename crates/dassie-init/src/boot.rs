//! Boot parameters the PID-1 layer reads from the kernel cmdline before it
//! switches into the app rootfs (ADR 0006), and where the guest drive is
//! (ADR 0015).

use std::fmt;

/// The guest drive: attached second, after the rootfs (ADR 0015).
pub const GUEST_DEVICE: &str = "/dev/vdb";

/// Where init mounts the guest drive in the initramfs, out of the app's
/// reach after the switch-root.
pub const GUEST_MOUNT: &str = "/guest";

/// What init takes from the kernel cmdline. Per-VM config, the guest's
/// address included, is on the guest drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmdline {
    pub root: RootSpec,
}

/// A `dassie.*` or `ip=` arg on the cmdline, from a host older than
/// ADR 0015.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyArg(pub String);

impl fmt::Display for LegacyArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "kernel cmdline has {:?}: per-VM config moved to the guest drive (ADR 0015)",
            self.0
        )
    }
}

impl std::error::Error for LegacyArg {}

impl Cmdline {
    /// Parses the cmdline the way the kernel does: the last occurrence of
    /// an arg wins. Any `dassie.*` or `ip=` arg is an error, so an old host
    /// with a new init fails loudly instead of having its config dropped.
    pub fn parse(text: &str) -> Result<Self, LegacyArg> {
        let mut root = RootSpec::default();
        for token in split_cmdline(text) {
            if token.starts_with("dassie.") || token.starts_with("ip=") {
                return Err(LegacyArg(token));
            }
            match token.split_once('=') {
                Some(("root", dev)) if !dev.is_empty() => dev.clone_into(&mut root.device),
                Some(("rootfstype", fs)) if !fs.is_empty() => fs.clone_into(&mut root.fstype),
                _ => {}
            }
        }
        Ok(Self { root })
    }
}

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

impl Default for RootSpec {
    fn default() -> Self {
        Self {
            device: "/dev/vda".to_owned(),
            fstype: "erofs".to_owned(),
        }
    }
}

/// Splits like the kernel does: on whitespace outside double quotes, with the
/// quotes removed (`a="b c"` -> `a=b c`).
fn split_cmdline(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut in_token = false;
    for c in text.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                in_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            c => {
                current.push(c);
                in_token = true;
            }
        }
    }
    if in_token {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Cmdline {
        Cmdline::parse(text).unwrap()
    }

    #[test]
    fn defaults_to_the_first_virtio_drive() {
        assert_eq!(
            parse("console=ttyS0 reboot=k panic=1 ro"),
            Cmdline {
                root: RootSpec {
                    device: "/dev/vda".into(),
                    fstype: "erofs".into(),
                },
            }
        );
    }

    #[test]
    fn last_occurrence_wins() {
        let cmdline = parse("root=/dev/vda rootfstype=xfs root=/dev/vdb rootfstype=ext4");
        assert_eq!(
            cmdline.root,
            RootSpec {
                device: "/dev/vdb".into(),
                fstype: "ext4".into(),
            }
        );
    }

    #[test]
    fn ignores_lookalikes_and_empty_values() {
        assert_eq!(
            parse("root= rootfstype= xroot=/dev/x rootwait rw xip=dhcp"),
            parse("")
        );
    }

    #[test]
    fn refuses_legacy_args() {
        assert_eq!(
            Cmdline::parse(r#"ro dassie.env="A=b c" root=/dev/vda"#),
            Err(LegacyArg("dassie.env=A=b c".into()))
        );
        assert!(Cmdline::parse("dassie.instance=web").is_err());
        assert_eq!(
            Cmdline::parse("ro ip=10.88.0.3::10.88.0.2:255.255.255.254::eth0:off"),
            Err(LegacyArg(
                "ip=10.88.0.3::10.88.0.2:255.255.255.254::eth0:off".into()
            ))
        );
    }

    #[test]
    fn splits_like_the_kernel() {
        assert_eq!(
            split_cmdline(" a  b=\"c d\" \"e f\"=g h\n"),
            ["a", "b=c d", "e f=g", "h"]
        );
        assert_eq!(split_cmdline("   "), Vec::<String>::new());
    }
}
