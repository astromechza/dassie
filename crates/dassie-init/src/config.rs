//! Builds the entrypoint's [`RunSpec`] from the image config and the guest
//! config (ADR 0005, ADR 0015).
//!
//! - `/.dassie/image.json` in the app rootfs is the image's full OCI config
//!   blob. Only `config.{Entrypoint, Cmd, Env, User, WorkingDir,
//!   ExposedPorts}` are honoured.
//! - The per-VM [`GuestConfig`] from the guest drive overrides the image
//!   config. Its hostname and nameservers also become `/etc/hostname`,
//!   `/etc/hosts` ([`hosts`]) and `/etc/resolv.conf` ([`resolv_conf`]).

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, io};

use dassie_proto::GuestConfig;
use serde::Deserialize;

use crate::user::{self, Accounts};

/// Image config path inside the app rootfs.
pub const IMAGE_CONFIG: &str = ".dassie/image.json";

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The hostname's address in `/etc/hosts` for a VM with no NIC, as Debian
/// maps its own name.
pub const NO_NIC_ADDRESS: Ipv4Addr = Ipv4Addr::new(127, 0, 1, 1);

/// Everything needed to spawn and supervise the entrypoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    /// Program and arguments: the guest or image `entrypoint`, then its
    /// `cmd`, by the Kubernetes rules (ADR 0015).
    pub argv: Vec<String>,
    /// The complete environment, in order.
    pub env: Vec<(String, String)>,
    /// `(uid, gid)` to switch to, or `None` to stay root.
    pub user: Option<(u32, u32)>,
    pub workdir: PathBuf,
    /// Port whose TCP connect means ready; `None` means ready on spawn.
    pub ready_port: Option<u16>,
    /// Addresses the readiness probe connects to: loopback, then the one
    /// `/etc/hosts` maps the hostname to, for an app that binds only there.
    pub ready_addrs: Vec<Ipv4Addr>,
    /// Default grace period between SIGTERM and SIGKILL.
    pub stop_timeout: Duration,
}

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, io::Error),
    Image(serde_json::Error),
    NoCommand,
    User(user::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, e) => write!(f, "read {}: {e}", path.display()),
            Self::Image(e) => write!(f, "invalid image config: {e}"),
            Self::NoCommand => write!(
                f,
                "no command: neither the guest nor the image config has one"
            ),
            Self::User(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Loads the spec for the app rootfs mounted at `root`, given the guest
/// config and the hostname's address ([`hosts`]). `/etc/passwd` and
/// `/etc/group` under `root` are optional.
pub fn load(root: &Path, guest: &GuestConfig, address: Ipv4Addr) -> Result<RunSpec, Error> {
    let read = |rel: &str| {
        let path = root.join(rel);
        fs::read_to_string(&path).map_err(|e| Error::Io(path, e))
    };
    let optional = |rel: &str| match read(rel) {
        Err(Error::Io(_, e)) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        other => other,
    };
    let image = read(IMAGE_CONFIG)?;
    let accounts = Accounts::parse(&optional("etc/passwd")?, &optional("etc/group")?);
    build(&image, guest, address, &accounts)
}

/// Builds the spec from the image config JSON, the guest config, the
/// hostname's address and the rootfs accounts. Guest config values win over
/// the image config.
pub fn build(
    image_json: &str,
    guest: &GuestConfig,
    address: Ipv4Addr,
    accounts: &Accounts,
) -> Result<RunSpec, Error> {
    let image: ImageFile = serde_json::from_str(image_json).map_err(Error::Image)?;
    let image = image.config;

    let argv = argv(image.entrypoint, image.cmd, guest);
    if argv.is_empty() {
        return Err(Error::NoCommand);
    }

    let account = match image.user.as_deref() {
        None | Some("") => None,
        Some(spec) => Some(accounts.resolve(spec).map_err(Error::User)?),
    };

    let mut env = Env::default();
    for entry in &image.env {
        let (k, v) = entry.split_once('=').unwrap_or((entry, ""));
        env.set(k, v);
    }
    for (k, v) in &guest.env {
        env.set(k, v);
    }
    env.set("DASSIE_INSTANCE_ID", &guest.instance_id);
    env.set_default("PATH", DEFAULT_PATH);
    let home = account
        .as_ref()
        .and_then(|a| a.home.as_deref())
        .unwrap_or("/");
    env.set_default("HOME", home);

    let ready_port = guest
        .readiness_port
        .filter(|&port| port != 0)
        .or_else(|| lowest_tcp_port(&image.exposed_ports));
    let mut ready_addrs = vec![Ipv4Addr::LOCALHOST];
    if address != Ipv4Addr::LOCALHOST {
        ready_addrs.push(address);
    }
    let workdir = match image.working_dir.as_deref() {
        None | Some("") => PathBuf::from("/"),
        Some(dir) => PathBuf::from(dir),
    };

    Ok(RunSpec {
        argv,
        env: env.0,
        user: account.map(|a| (a.uid, a.gid)),
        workdir,
        ready_port,
        ready_addrs,
        stop_timeout: guest.sigterm_timeout(),
    })
}

/// argv by the Kubernetes rules: the guest `entrypoint` replaces the
/// image's and the guest `cmd` replaces the image's, except that a guest
/// `entrypoint` with no `cmd` also drops the image `Cmd`.
fn argv(entrypoint: Vec<String>, cmd: Vec<String>, guest: &GuestConfig) -> Vec<String> {
    match (guest.entrypoint.is_empty(), guest.cmd.is_empty()) {
        (true, true) => entrypoint.into_iter().chain(cmd).collect(),
        (true, false) => entrypoint.into_iter().chain(guest.cmd.clone()).collect(),
        (false, _) => guest.entrypoint.iter().chain(&guest.cmd).cloned().collect(),
    }
}

const WRITTEN_BY: &str = "# Written by dassie-init from the guest config (ADR 0015).\n";

/// `/etc/hosts` contents: the usual `localhost` lines, and `hostname` at
/// `address`, the guest's own address or [`NO_NIC_ADDRESS`].
pub fn hosts(hostname: &str, address: Ipv4Addr) -> String {
    format!(
        "{WRITTEN_BY}127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n{address}\t{hostname}\n"
    )
}

/// `/etc/resolv.conf` contents for the given resolvers.
pub fn resolv_conf(nameservers: &[IpAddr]) -> String {
    let mut out = String::from(WRITTEN_BY);
    for ip in nameservers {
        // Writing to a String cannot fail.
        let _ = writeln!(out, "nameserver {ip}");
    }
    out
}

/// `ExposedPorts` keys are `PORT[/PROTO]`; a missing proto means tcp. Port 0
/// is skipped, as for the guest's `readiness_port`: nothing can be probed
/// there.
fn lowest_tcp_port(exposed: &BTreeMap<String, serde_json::Value>) -> Option<u16> {
    exposed
        .keys()
        .filter_map(|key| match key.split_once('/') {
            None => key.parse::<u16>().ok(),
            Some((port, "tcp")) => port.parse().ok(),
            Some(_) => None,
        })
        .filter(|&port| port != 0)
        .min()
}

/// Ordered environment; setting an existing key replaces it in place.
#[derive(Default)]
struct Env(Vec<(String, String)>);

impl Env {
    fn set(&mut self, key: &str, value: &str) {
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => value.clone_into(&mut entry.1),
            None => self.0.push((key.to_owned(), value.to_owned())),
        }
    }

    fn set_default(&mut self, key: &str, value: &str) {
        if !self.0.iter().any(|(k, _)| k == key) {
            self.set(key, value);
        }
    }
}

#[derive(Deserialize)]
struct ImageFile {
    #[serde(default)]
    config: ImageConfig,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageConfig {
    #[serde(default, deserialize_with = "null_as_default")]
    entrypoint: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    cmd: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    env: Vec<String>,
    user: Option<String>,
    working_dir: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    exposed_ports: BTreeMap<String, serde_json::Value>,
}

/// OCI configs written by Docker use `null` for empty lists and maps.
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\napp:x:1000:1000::/home/app:/bin/sh\n";
    const GROUP: &str = "root:x:0:\napp:x:1000:\nwww:x:33:\n";

    fn accounts() -> Accounts {
        Accounts::parse(PASSWD, GROUP)
    }

    fn image(config: &str) -> String {
        format!(
            r#"{{"architecture":"amd64","os":"linux","config":{config},"rootfs":{{"type":"layers","diff_ids":[]}}}}"#
        )
    }

    fn guest() -> GuestConfig {
        GuestConfig {
            instance_id: "web-a".into(),
            ..GuestConfig::default()
        }
    }

    fn spec(image_config: &str, guest: &GuestConfig) -> Result<RunSpec, Error> {
        build(&image(image_config), guest, NO_NIC_ADDRESS, &accounts())
    }

    #[test]
    fn minimal_image_gets_defaults() {
        assert_eq!(
            spec(r#"{"Cmd":["/app"]}"#, &guest()).unwrap(),
            RunSpec {
                argv: vec!["/app".into()],
                env: vec![
                    ("DASSIE_INSTANCE_ID".into(), "web-a".into()),
                    ("PATH".into(), DEFAULT_PATH.into()),
                    ("HOME".into(), "/".into())
                ],
                user: None,
                workdir: "/".into(),
                ready_port: None,
                ready_addrs: vec![Ipv4Addr::LOCALHOST, NO_NIC_ADDRESS],
                stop_timeout: Duration::from_secs(10),
            }
        );
    }

    #[test]
    fn docker_nulls_are_empty() {
        let spec = spec(
            r#"{"Entrypoint":null,"Cmd":["/app"],"Env":null,"ExposedPorts":null,"User":"","WorkingDir":""}"#,
            &guest(),
        )
        .unwrap();
        assert_eq!(spec.argv, ["/app"]);
        assert_eq!(spec.user, None);
    }

    #[test]
    fn argv_follows_the_kubernetes_rules() {
        let image = r#"{"Entrypoint":["/bin/server","--flag"],"Cmd":["serve"]}"#;
        let strings = |s: &[&str]| s.iter().map(|&s| s.to_owned()).collect::<Vec<_>>();
        let cases: [(&[&str], &[&str], &[&str]); 4] = [
            (&[], &[], &["/bin/server", "--flag", "serve"]),
            (
                &[],
                &["run", "a b"],
                &["/bin/server", "--flag", "run", "a b"],
            ),
            (&["/bin/other"], &[], &["/bin/other"]),
            (&["/bin/other"], &["x"], &["/bin/other", "x"]),
        ];
        for (entrypoint, cmd, want) in cases {
            let guest = GuestConfig {
                entrypoint: strings(entrypoint),
                cmd: strings(cmd),
                ..guest()
            };
            assert_eq!(spec(image, &guest).unwrap().argv, want, "{guest:?}");
        }
    }

    #[test]
    fn no_command_is_an_error() {
        assert!(matches!(spec("{}", &guest()), Err(Error::NoCommand)));
    }

    #[test]
    fn guest_overrides_image() {
        let guest = GuestConfig {
            env: [
                ("A", "guest"),
                ("C", "with space"),
                ("DASSIE_INSTANCE_ID", "spoofed"),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .into(),
            readiness_port: Some(7000),
            sigterm_timeout_seconds: Some(3),
            ..guest()
        };
        let spec = spec(
            r#"{"Cmd":["/app"],"Env":["PATH=/bin","A=image","B=image"],"User":"app","WorkingDir":"/srv","ExposedPorts":{"9000/tcp":{},"8080/tcp":{},"53/udp":{}}}"#,
            &guest,
        )
        .unwrap();
        assert_eq!(
            spec.env,
            [
                ("PATH", "/bin"),
                ("A", "guest"),
                ("B", "image"),
                ("C", "with space"),
                ("DASSIE_INSTANCE_ID", "web-a"),
                ("HOME", "/home/app"),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
        );
        assert_eq!(spec.user, Some((1000, 1000)));
        assert_eq!(spec.workdir, PathBuf::from("/srv"));
        assert_eq!(spec.ready_port, Some(7000));
        assert_eq!(spec.stop_timeout, Duration::from_secs(3));
    }

    #[test]
    fn ready_port_is_lowest_exposed_tcp() {
        let cases = [
            (r#"{"9000/tcp":{},"8080/tcp":{},"53/udp":{}}"#, Some(8080)),
            (r#"{"80":{}}"#, Some(80)),
            (r#"{"53/udp":{}}"#, None),
            (r#"{"0/tcp":{},"8080/tcp":{}}"#, Some(8080)),
            (r#"{"0":{}}"#, None),
            ("{}", None),
        ];
        for (ports, want) in cases {
            let config = format!(r#"{{"Cmd":["/app"],"ExposedPorts":{ports}}}"#);
            assert_eq!(spec(&config, &guest()).unwrap().ready_port, want, "{ports}");
        }
    }

    #[test]
    fn readiness_also_probes_the_hostname_address() {
        let address = Ipv4Addr::new(172, 16, 0, 3);
        let spec = build(
            &image(r#"{"Cmd":["/app"]}"#),
            &guest(),
            address,
            &accounts(),
        )
        .unwrap();
        assert_eq!(spec.ready_addrs, [Ipv4Addr::LOCALHOST, address]);
        let spec = build(
            &image(r#"{"Cmd":["/app"]}"#),
            &guest(),
            Ipv4Addr::LOCALHOST,
            &accounts(),
        )
        .unwrap();
        assert_eq!(spec.ready_addrs, [Ipv4Addr::LOCALHOST]);
    }

    #[test]
    fn writes_etc_files() {
        assert_eq!(
            hosts("web-a", Ipv4Addr::new(172, 16, 0, 3)),
            "# Written by dassie-init from the guest config (ADR 0015).\n\
             127.0.0.1\tlocalhost\n\
             ::1\tlocalhost ip6-localhost ip6-loopback\n\
             172.16.0.3\tweb-a\n"
        );
        let nameservers = ["1.1.1.1", "2606:4700::1111"].map(|ip| ip.parse::<IpAddr>().unwrap());
        assert_eq!(
            resolv_conf(&nameservers),
            "# Written by dassie-init from the guest config (ADR 0015).\n\
             nameserver 1.1.1.1\n\
             nameserver 2606:4700::1111\n"
        );
    }

    #[test]
    fn load_reads_rootfs_files() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".dassie")).unwrap();
        fs::write(
            root.path().join(IMAGE_CONFIG),
            image(r#"{"Cmd":["/app"],"User":"1000"}"#),
        )
        .unwrap();
        // No /etc/passwd: numeric users still resolve.
        let spec = load(root.path(), &guest(), NO_NIC_ADDRESS).unwrap();
        assert_eq!(spec.user, Some((1000, 0)));

        let missing = tempfile::tempdir().unwrap();
        assert!(matches!(
            load(missing.path(), &guest(), NO_NIC_ADDRESS),
            Err(Error::Io(..))
        ));
    }
}
