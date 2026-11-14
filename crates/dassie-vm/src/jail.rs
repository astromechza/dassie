//! Running Firecracker under its `jailer` (ADR 0013).
//!
//! vm-run builds a fresh chroot for every boot: the VM's assets hard-linked
//! in under fixed names, and a config whose paths point at them. The jailer
//! then adds the device nodes, drops to the VM's uid and gid, and execs
//! Firecracker with the same pid.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// Where chroots live: on the same filesystem as `/var/lib/dassie/guest` and
/// `/var/lib/dassie/rootfs`, so assets can be hard-linked.
pub const DEFAULT_CHROOT_BASE: &str = "/var/lib/dassie/jail";

/// Firecracker's config file inside the jail.
pub const CONFIG: &str = "/config.json";

/// The vsock device's `uds_path` inside the jail.
pub const VSOCK: &str = "/vsock";

/// One VM's jail.
#[derive(Debug, Clone)]
pub struct Jail {
    /// `<chroot base>/<exec file name>/<id>`: everything the jailer and
    /// vm-run create for this VM.
    dir: PathBuf,
    id: String,
    uid: u32,
    gid: u32,
    firecracker: PathBuf,
}

impl Jail {
    /// Describes the jail for instance `id`, without touching it.
    ///
    /// `firecracker` is resolved through `PATH` when it has no `/`, then
    /// canonicalized as the jailer does, because the jailer names the chroot
    /// after its file name. `chroot_base` is created if missing.
    pub fn new(
        chroot_base: &Path,
        id: &str,
        uid: u32,
        gid: u32,
        firecracker: &Path,
    ) -> io::Result<Self> {
        validate_id(id)?;
        let firecracker = resolve_exec(firecracker)?;
        let name = firecracker
            .file_name()
            .ok_or_else(|| invalid(format!("{}: no file name", firecracker.display())))?;
        fs::create_dir_all(chroot_base).map_err(|e| ctx(&e, "create", chroot_base))?;
        let base = fs::canonicalize(chroot_base).map_err(|e| ctx(&e, "resolve", chroot_base))?;
        Ok(Self {
            dir: base.join(name).join(id),
            id: id.to_owned(),
            uid,
            gid,
            firecracker,
        })
    }

    /// The jail's root on the host: what the guest's paths are relative to.
    pub fn root(&self) -> PathBuf {
        self.dir.join("root")
    }

    /// The vsock device's socket on the host: stable for the instance.
    pub fn vsock(&self) -> PathBuf {
        self.root().join(VSOCK.trim_start_matches('/'))
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn gid(&self) -> u32 {
        self.gid
    }

    /// Replaces whatever a previous boot left with a fresh root holding the
    /// VM's assets and its rewritten config.
    ///
    /// `host_config` is a Firecracker config with host paths, as
    /// `vm-config.sh` writes it. The caller must first make sure no previous
    /// boot is still running (its vsock sockets are stale).
    pub fn prepare(&self, host_config: &Path) -> io::Result<()> {
        let bytes = fs::read(host_config).map_err(|e| ctx(&e, "read", host_config))?;
        let config: Value = serde_json::from_slice(&bytes)
            .map_err(|e| invalid(format!("parse {}: {e}", host_config.display())))?;
        let (config, links) =
            jail_config(config).map_err(|e| invalid(format!("{}: {e}", host_config.display())))?;

        match fs::remove_dir_all(&self.dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(ctx(&e, "remove stale jail", &self.dir));
            }
            _ => {}
        }
        let root = self.root();
        fs::create_dir_all(&root).map_err(|e| ctx(&e, "create", &root))?;
        for link in links {
            let target = root.join(&link.name);
            // Link the file itself: on Linux, `link(2)` of a symlink links the
            // symlink, whose target the chroot cannot see.
            let linked = fs::canonicalize(&link.host).and_then(|host| fs::hard_link(host, &target));
            linked.map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "link {} into the jail as {}: {e} (assets must be on the jail's filesystem)",
                        link.host.display(),
                        target.display()
                    ),
                )
            })?;
        }
        let config_path = root.join(CONFIG.trim_start_matches('/'));
        let json = serde_json::to_vec_pretty(&config).map_err(io::Error::other)?;
        fs::write(&config_path, json).map_err(|e| ctx(&e, "write", &config_path))
    }

    /// The jailer invocation that boots the prepared jail.
    pub fn command(&self, jailer: &Path) -> Command {
        let mut cmd = Command::new(jailer);
        cmd.args(self.jailer_args());
        cmd
    }

    fn jailer_args(&self) -> Vec<OsString> {
        let base = self
            .dir
            .parent()
            .and_then(Path::parent)
            .unwrap_or(Path::new("/"));
        [
            "--id".into(),
            self.id.clone().into(),
            "--exec-file".into(),
            self.firecracker.clone().into(),
            "--uid".into(),
            self.uid.to_string().into(),
            "--gid".into(),
            self.gid.to_string().into(),
            "--chroot-base-dir".into(),
            base.into(),
            // No --cgroup flags: Firecracker stays in the systemd unit's
            // cgroup. No --daemonize or --new-pid-ns: the jailer execs
            // Firecracker with its own pid, which vm-run waits on.
            "--".into(),
            "--no-api".into(),
            "--config-file".into(),
            CONFIG.into(),
        ]
        .into()
    }
}

/// A host file to hard-link into the jail root as `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub host: PathBuf,
    pub name: String,
}

/// Rewrites a host-path Firecracker config for the jail. It returns the new
/// config and the files to link in:
///
/// - `boot-source.kernel_image_path` → `/kernel`;
/// - `boot-source.initrd_path` → `/initrd`;
/// - each `drives[].path_on_host` → `/drive-<drive_id>`;
/// - `vsock.uds_path` → [`VSOCK`] (nothing to link: Firecracker creates it).
///
/// Refuses configs with other host paths (`logger`, `metrics`) rather than
/// boot with paths that do not exist in the jail.
pub fn jail_config(mut config: Value) -> Result<(Value, Vec<Link>), String> {
    for key in ["logger", "metrics"] {
        if config.get(key).is_some() {
            return Err(format!("`{key}` is not supported in a jailed VM"));
        }
    }
    let mut links = Vec::new();
    let mut relink = |value: Option<&mut Value>, name: String, required: bool| match value {
        Some(Value::String(path)) => {
            links.push(Link {
                host: PathBuf::from(&*path),
                name: name.clone(),
            });
            *path = format!("/{name}");
            Ok(())
        }
        None if !required => Ok(()),
        _ => Err(format!("missing or non-string path for /{name}")),
    };
    let boot = config.get_mut("boot-source");
    let Some(boot) = boot else {
        return Err("no `boot-source`".into());
    };
    relink(boot.get_mut("kernel_image_path"), "kernel".into(), true)?;
    relink(boot.get_mut("initrd_path"), "initrd".into(), false)?;
    if let Some(drives) = config.get_mut("drives").and_then(Value::as_array_mut) {
        for drive in drives {
            let id = drive
                .get("drive_id")
                .and_then(Value::as_str)
                .filter(|id| {
                    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
                .ok_or("a drive without a valid `drive_id`")?
                .to_owned();
            relink(drive.get_mut("path_on_host"), format!("drive-{id}"), true)?;
        }
    }
    match config.get_mut("vsock").and_then(|v| v.get_mut("uds_path")) {
        Some(path @ Value::String(_)) => *path = Value::String(VSOCK.into()),
        _ => return Err("no `vsock.uds_path`".into()),
    }
    Ok((config, links))
}

/// The jailer's rule for `--id`: 1–64 characters, alphanumeric or `-`.
/// Stricter here (ASCII only), so an id is always a single safe path
/// component.
fn validate_id(id: &str) -> io::Result<()> {
    if !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        Ok(())
    } else {
        Err(invalid(format!(
            "jail id {id:?} must be 1-64 ASCII letters, digits or -"
        )))
    }
}

/// Resolves a binary as `Command` would (through `PATH` when the name has no
/// `/`), then canonicalizes it.
fn resolve_exec(bin: &Path) -> io::Result<PathBuf> {
    let found = if bin.components().count() > 1 {
        Some(bin.to_owned())
    } else {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(bin))
                .find(|p| p.is_file())
        })
    };
    let found = found.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} not found in PATH", bin.display()),
        )
    })?;
    fs::canonicalize(&found).map_err(|e| ctx(&e, "resolve", &found))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn ctx(e: &io::Error, what: &str, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{what} {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use serde_json::json;

    use super::*;

    fn host_config(dir: &Path) -> Value {
        json!({
            "boot-source": {
                "kernel_image_path": dir.join("vmlinux"),
                "initrd_path": dir.join("initramfs.cpio.gz"),
                "boot_args": "console=ttyS0"
            },
            "drives": [{
                "drive_id": "rootfs",
                "path_on_host": dir.join("rootfs.erofs"),
                "is_read_only": true
            }],
            "machine-config": { "vcpu_count": 1, "mem_size_mib": 256 },
            "vsock": { "guest_cid": 3, "uds_path": "/run/dassie/a.vsock" },
            "network-interfaces": [{ "iface_id": "eth0", "host_dev_name": "dt00001" }]
        })
    }

    #[test]
    fn rewrites_host_paths_to_jail_paths() {
        let (config, links) = jail_config(host_config(Path::new("/assets"))).unwrap();
        assert_eq!(config["boot-source"]["kernel_image_path"], "/kernel");
        assert_eq!(config["boot-source"]["initrd_path"], "/initrd");
        assert_eq!(config["drives"][0]["path_on_host"], "/drive-rootfs");
        assert_eq!(config["vsock"]["uds_path"], "/vsock");
        // Everything else is Firecracker's business and passes through.
        assert_eq!(config["boot-source"]["boot_args"], "console=ttyS0");
        assert_eq!(config["network-interfaces"][0]["host_dev_name"], "dt00001");
        assert_eq!(
            links,
            [
                Link {
                    host: "/assets/vmlinux".into(),
                    name: "kernel".into()
                },
                Link {
                    host: "/assets/initramfs.cpio.gz".into(),
                    name: "initrd".into()
                },
                Link {
                    host: "/assets/rootfs.erofs".into(),
                    name: "drive-rootfs".into()
                },
            ]
        );
    }

    #[test]
    fn refuses_configs_it_cannot_jail() {
        type Break = fn(&mut Value);
        let dir = Path::new("/assets");
        let cases: [(&str, Break); 5] = [
            ("no `vsock.uds_path`", |c| {
                c.as_object_mut().unwrap().remove("vsock");
            }),
            ("/kernel", |c| {
                c["boot-source"]["kernel_image_path"] = json!(null);
            }),
            ("drive_id", |c| c["drives"][0]["drive_id"] = json!("../etc")),
            ("`logger`", |c| c["logger"] = json!({ "log_path": "/x" })),
            ("no `boot-source`", |c| {
                c.as_object_mut().unwrap().remove("boot-source");
            }),
        ];
        for (want, break_it) in cases {
            let mut config = host_config(dir);
            break_it(&mut config);
            let err = jail_config(config).unwrap_err();
            assert!(err.contains(want), "{err} should mention {want}");
        }
    }

    #[test]
    fn validates_ids() {
        for ok in ["a", "web-1", &"x".repeat(64)] {
            validate_id(ok).unwrap();
        }
        for bad in ["", "../x", "a/b", "a.b", "é", &"x".repeat(65)] {
            validate_id(bad).unwrap_err();
        }
    }

    #[test]
    fn prepares_a_fresh_jail_each_time() {
        let dir = tempfile::tempdir().unwrap();
        let assets = dir.path().join("assets");
        fs::create_dir(&assets).unwrap();
        for name in ["vmlinux-6.1", "initramfs.cpio.gz", "rootfs.erofs"] {
            fs::write(assets.join(name), name).unwrap();
        }
        // A symlinked asset is linked in as the file it points to.
        std::os::unix::fs::symlink("vmlinux-6.1", assets.join("vmlinux")).unwrap();
        let config = dir.path().join("config.json");
        fs::write(&config, host_config(&assets).to_string()).unwrap();
        let firecracker = assets.join("firecracker");
        fs::write(&firecracker, b"").unwrap();

        let jail = Jail::new(&dir.path().join("jail"), "vm-1", 7, 8, &firecracker).unwrap();
        let root = jail.root();
        assert!(
            root.ends_with("jail/firecracker/vm-1/root"),
            "{}",
            root.display()
        );
        assert_eq!(jail.vsock(), root.join("vsock"));

        jail.prepare(&config).unwrap();
        // A crashed boot's leftovers, which the jailer would trip on.
        fs::create_dir(root.join("dev")).unwrap();
        fs::write(root.join("firecracker.pid"), b"1").unwrap();
        jail.prepare(&config).unwrap();

        let mut names: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["config.json", "drive-rootfs", "initrd", "kernel"]);
        // Hard links: the very same file, not a copy.
        let ino = |p: &Path| fs::metadata(p).unwrap().ino();
        assert_eq!(
            ino(&root.join("drive-rootfs")),
            ino(&assets.join("rootfs.erofs"))
        );
        let kernel = fs::symlink_metadata(root.join("kernel")).unwrap();
        assert!(kernel.is_file());
        assert_eq!(kernel.ino(), ino(&assets.join("vmlinux-6.1")));
        let jailed: Value =
            serde_json::from_slice(&fs::read(root.join("config.json")).unwrap()).unwrap();
        assert_eq!(jailed["vsock"]["uds_path"], "/vsock");

        let args: Vec<String> = jail
            .jailer_args()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        let base = fs::canonicalize(dir.path().join("jail")).unwrap();
        let fc = fs::canonicalize(&firecracker).unwrap();
        assert_eq!(
            args,
            [
                "--id",
                "vm-1",
                "--exec-file",
                fc.to_str().unwrap(),
                "--uid",
                "7",
                "--gid",
                "8",
                "--chroot-base-dir",
                base.to_str().unwrap(),
                "--",
                "--no-api",
                "--config-file",
                "/config.json"
            ]
        );
    }

    #[test]
    fn a_missing_asset_fails_the_prepare() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        fs::write(
            &config,
            host_config(&dir.path().join("nowhere")).to_string(),
        )
        .unwrap();
        let fc = dir.path().join("firecracker");
        fs::write(&fc, b"").unwrap();
        let jail = Jail::new(&dir.path().join("jail"), "vm-1", 7, 8, &fc).unwrap();
        let err = jail.prepare(&config).unwrap_err();
        assert!(err.to_string().contains("into the jail"), "{err}");
    }
}
