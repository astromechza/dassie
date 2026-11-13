//! Builds the entrypoint's [`RunSpec`] from the image config and the kernel
//! cmdline (ADR 0005).
//!
//! - `/.dassie/image.json` in the app rootfs is the image's full OCI config
//!   blob. Only `config.{Entrypoint, Cmd, Env, User, WorkingDir,
//!   ExposedPorts}` are honoured.
//! - Per-VM `dassie.*` kernel cmdline args override the image config.
//! - `dassie.dns` also carries the guest's resolvers, which init writes to
//!   `/etc/resolv.conf` ([`resolv_conf`]).

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, io};

use serde::Deserialize;

use crate::user::{self, Accounts};

/// Image config path inside the app rootfs.
pub const IMAGE_CONFIG: &str = ".dassie/image.json";

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Resolvers beyond this many are ignored by both glibc and musl (`MAXNS`).
const MAX_DNS: usize = 3;

/// Everything needed to spawn and supervise the entrypoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    /// Program and arguments: `Entrypoint` followed by `Cmd`.
    pub argv: Vec<String>,
    /// The complete environment, in order.
    pub env: Vec<(String, String)>,
    /// `(uid, gid)` to switch to, or `None` to stay root.
    pub user: Option<(u32, u32)>,
    pub workdir: PathBuf,
    /// Port whose TCP connect means ready; `None` means ready on spawn.
    pub ready_port: Option<u16>,
    /// Default grace period between SIGTERM and SIGKILL.
    pub stop_timeout: Duration,
    /// Resolvers from `dassie.dns`, in order. Empty leaves the image's
    /// `/etc/resolv.conf` alone.
    pub dns: Vec<IpAddr>,
}

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, io::Error),
    Image(serde_json::Error),
    NoCommand,
    Cmdline { key: String, value: String },
    User(user::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, e) => write!(f, "read {}: {e}", path.display()),
            Self::Image(e) => write!(f, "invalid image config: {e}"),
            Self::NoCommand => write!(f, "image config has no Entrypoint or Cmd"),
            Self::Cmdline { key, value } => write!(f, "invalid cmdline value {key}={value:?}"),
            Self::User(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Loads the spec for the app rootfs mounted at `root`, given the kernel
/// cmdline text. `/etc/passwd` and `/etc/group` under `root` are optional.
pub fn load(root: &Path, cmdline: &str) -> Result<RunSpec, Error> {
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
    build(&image, cmdline, &accounts)
}

/// Builds the spec from the image config JSON, the kernel cmdline text and the
/// rootfs accounts. Cmdline values win over the image config.
pub fn build(image_json: &str, cmdline: &str, accounts: &Accounts) -> Result<RunSpec, Error> {
    let image: ImageFile = serde_json::from_str(image_json).map_err(Error::Image)?;
    let image = image.config;
    let cmdline = Cmdline::parse(cmdline)?;

    let argv: Vec<String> = image.entrypoint.into_iter().chain(image.cmd).collect();
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
    for (k, v) in &cmdline.env {
        env.set(k, v);
    }
    if let Some(instance) = &cmdline.instance {
        env.set("DASSIE_INSTANCE", instance);
    }
    if let Some(colour) = &cmdline.colour {
        env.set("DASSIE_COLOUR", colour);
    }
    env.set_default("PATH", DEFAULT_PATH);
    let home = account
        .as_ref()
        .and_then(|a| a.home.as_deref())
        .unwrap_or("/");
    env.set_default("HOME", home);

    let ready_port = cmdline
        .port
        .or_else(|| lowest_tcp_port(&image.exposed_ports));
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
        stop_timeout: cmdline.stop_timeout.unwrap_or(DEFAULT_STOP_TIMEOUT),
        dns: cmdline.dns,
    })
}

/// `/etc/resolv.conf` contents for the given resolvers.
pub fn resolv_conf(dns: &[IpAddr]) -> String {
    let mut out = String::from("# Written by dassie-init from dassie.dns (ADR 0005).\n");
    for ip in dns {
        // Writing to a String cannot fail.
        let _ = writeln!(out, "nameserver {ip}");
    }
    out
}

/// `ExposedPorts` keys are `PORT[/PROTO]`; a missing proto means tcp. Port 0
/// is skipped, as for `dassie.port`: nothing can be probed there.
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

/// The `dassie.*` args from the kernel cmdline. Unknown `dassie.*` keys are
/// ignored so a newer host can pass args an older init does not know.
#[derive(Debug, Default, PartialEq, Eq)]
struct Cmdline {
    instance: Option<String>,
    colour: Option<String>,
    env: Vec<(String, String)>,
    port: Option<u16>,
    stop_timeout: Option<Duration>,
    dns: Vec<IpAddr>,
}

impl Cmdline {
    fn parse(text: &str) -> Result<Self, Error> {
        let mut out = Self::default();
        for token in split_cmdline(text) {
            let Some((key, value)) = token.split_once('=') else {
                continue;
            };
            let bad = || Error::Cmdline {
                key: key.to_owned(),
                value: value.to_owned(),
            };
            match key {
                "dassie.instance" => out.instance = Some(value.to_owned()),
                "dassie.colour" => out.colour = Some(value.to_owned()),
                "dassie.env" => {
                    let (k, v) = value.split_once('=').ok_or_else(bad)?;
                    out.env.push((k.to_owned(), v.to_owned()));
                }
                "dassie.port" => match value.parse() {
                    Ok(0) | Err(_) => return Err(bad()),
                    Ok(port) => out.port = Some(port),
                },
                "dassie.stop_timeout" => {
                    let secs = value.parse().map_err(|_| bad())?;
                    out.stop_timeout = Some(Duration::from_secs(secs));
                }
                "dassie.dns" => {
                    let dns = value
                        .split(',')
                        .map(str::parse)
                        .collect::<Result<Vec<IpAddr>, _>>()
                        .map_err(|_| bad())?;
                    if dns.len() > MAX_DNS {
                        return Err(bad());
                    }
                    out.dns = dns;
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

/// Splits like the kernel does: on whitespace outside double quotes, with the
/// quotes removed (`dassie.env="A=b c"` -> `dassie.env=A=b c`).
pub(crate) fn split_cmdline(text: &str) -> Vec<String> {
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

    #[test]
    fn minimal_image_gets_defaults() {
        let spec = build(&image(r#"{"Cmd":["/app"]}"#), "", &accounts()).unwrap();
        assert_eq!(
            spec,
            RunSpec {
                argv: vec!["/app".into()],
                env: vec![
                    ("PATH".into(), DEFAULT_PATH.into()),
                    ("HOME".into(), "/".into())
                ],
                user: None,
                workdir: "/".into(),
                ready_port: None,
                stop_timeout: DEFAULT_STOP_TIMEOUT,
                dns: vec![],
            }
        );
    }

    #[test]
    fn docker_nulls_are_empty() {
        let json = image(
            r#"{"Entrypoint":null,"Cmd":["/app"],"Env":null,"ExposedPorts":null,"User":"","WorkingDir":""}"#,
        );
        let spec = build(&json, "", &accounts()).unwrap();
        assert_eq!(spec.argv, ["/app"]);
        assert_eq!(spec.user, None);
    }

    #[test]
    fn argv_is_entrypoint_then_cmd() {
        let json = image(r#"{"Entrypoint":["/bin/server","--flag"],"Cmd":["serve"]}"#);
        let spec = build(&json, "", &accounts()).unwrap();
        assert_eq!(spec.argv, ["/bin/server", "--flag", "serve"]);
    }

    #[test]
    fn no_command_is_an_error() {
        let err = build(&image("{}"), "", &accounts()).unwrap_err();
        assert!(matches!(err, Error::NoCommand));
    }

    #[test]
    fn cmdline_overrides_image() {
        let json = image(
            r#"{"Cmd":["/app"],"Env":["PATH=/bin","A=image","B=image"],"User":"app","WorkingDir":"/srv","ExposedPorts":{"9000/tcp":{},"8080/tcp":{},"53/udp":{}}}"#,
        );
        let cmdline = r#"console=ttyS0 reboot=k dassie.instance=web-a dassie.colour=blue dassie.env=A=cmdline dassie.env="C=with space" dassie.port=7000 dassie.stop_timeout=3 dassie.future=x"#;
        let spec = build(&json, cmdline, &accounts()).unwrap();
        assert_eq!(
            spec.env,
            [
                ("PATH", "/bin"),
                ("A", "cmdline"),
                ("B", "image"),
                ("C", "with space"),
                ("DASSIE_INSTANCE", "web-a"),
                ("DASSIE_COLOUR", "blue"),
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
            let json = image(&format!(r#"{{"Cmd":["/app"],"ExposedPorts":{ports}}}"#));
            assert_eq!(
                build(&json, "", &accounts()).unwrap().ready_port,
                want,
                "{ports}"
            );
        }
    }

    #[test]
    fn bad_cmdline_values_are_errors() {
        for cmdline in [
            "dassie.port=http",
            "dassie.port=0",
            "dassie.stop_timeout=-1",
            "dassie.env=NOEQUALS",
            "dassie.dns=",
            "dassie.dns=1.1.1.1,",
            "dassie.dns=example.com",
            "dassie.dns=1.1.1.1,1.0.0.1,9.9.9.9,8.8.8.8",
        ] {
            let err = build(&image(r#"{"Cmd":["/app"]}"#), cmdline, &accounts()).unwrap_err();
            assert!(matches!(err, Error::Cmdline { .. }), "{cmdline}");
        }
    }

    #[test]
    fn dns_last_occurrence_wins() {
        let json = image(r#"{"Cmd":["/app"]}"#);
        let spec = build(
            &json,
            "dassie.dns=192.0.2.1 dassie.dns=1.1.1.1,2606:4700::1111",
            &accounts(),
        )
        .unwrap();
        assert_eq!(
            spec.dns,
            ["1.1.1.1", "2606:4700::1111"].map(|ip| ip.parse::<IpAddr>().unwrap())
        );
        assert_eq!(
            resolv_conf(&spec.dns),
            "# Written by dassie-init from dassie.dns (ADR 0005).\nnameserver 1.1.1.1\nnameserver 2606:4700::1111\n"
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
        let spec = load(root.path(), "").unwrap();
        assert_eq!(spec.user, Some((1000, 0)));

        let missing = tempfile::tempdir().unwrap();
        assert!(matches!(load(missing.path(), ""), Err(Error::Io(..))));
    }
}
