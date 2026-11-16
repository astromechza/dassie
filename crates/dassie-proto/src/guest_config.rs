//! The per-VM guest config: `guest.json` on the guest drive (ADR 0015).
//!
//! The host writes it with [`GuestConfig::encode`] and the guest reads it
//! with [`GuestConfig::decode`]. Both run [`GuestConfig::validate`], so the
//! host never writes a config the guest would refuse.
//!
//! Decoding is strict: `version` must be [`CONFIG_VERSION`], checked before
//! anything else, and unknown fields are errors. A newer host therefore
//! never has a field silently ignored by an older guest.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// The `version` this code reads and writes.
pub const CONFIG_VERSION: u64 = 1;

/// The config file's name, at the root of the guest drive and next to it
/// on the host.
pub const GUEST_FILE: &str = "guest.json";

/// The guest drive's Firecracker `drive_id`.
pub const DRIVE_ID: &str = "guest";

/// Largest `guest.json` either side accepts, in bytes.
pub const MAX_CONFIG_BYTES: usize = 1 << 20;

/// Linux's `MAX_ARG_STRLEN`: every argument and every `KEY=value` env entry
/// must be shorter, counting its terminating NUL.
pub const MAX_ARG_STRLEN: usize = 128 * 1024;

/// What `execve` allows argv and env together with the default 8 MiB stack
/// rlimit: a quarter of it, counted with [`exec_cost`].
pub const EXEC_LIMIT: usize = 2 << 20;

/// Most bytes `env`, `entrypoint` and `cmd` may take together, counted with
/// [`exec_cost`]: half of [`EXEC_LIMIT`], so the image's own `Env` and
/// `Cmd` keep the other half. Init checks the merged total at boot.
pub const MAX_EXEC_BYTES: usize = EXEC_LIMIT / 2;

/// What one argv or env string of `len` bytes takes on the new process's
/// stack: the bytes, its NUL and its 8-byte pointer.
pub const fn exec_cost(len: usize) -> usize {
    len + 1 + size_of::<u64>()
}

/// Longest `sigterm_timeout_seconds`.
pub const MAX_SIGTERM_TIMEOUT_SECONDS: u64 = 10;

/// The grace period when `sigterm_timeout_seconds` is unset.
pub const DEFAULT_SIGTERM_TIMEOUT_SECONDS: u64 = 10;

/// Longest hostname Linux accepts (`HOST_NAME_MAX`).
pub const MAX_HOSTNAME: usize = 64;

/// Most resolvers glibc and musl use (`MAXNS`).
pub const MAX_NAMESERVERS: usize = 3;

/// The only prefix length `ipv4_address` takes: each VM's TAP is a
/// point-to-point link holding the guest and the host (ADR 0008).
pub const IPV4_PREFIX: u8 = 31;

/// Per-VM values for `dassie-init`. Empty lists and maps mean unset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestConfig {
    /// `DASSIE_INSTANCE_ID` in the app env, and the default hostname, so it
    /// follows the same rules.
    pub instance_id: String,
    /// The guest's hostname: 1..=[`MAX_HOSTNAME`] ASCII letters, digits,
    /// `-` or `.`. Unset: `instance_id`; see [`GuestConfig::hostname`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Merged into the image `Env` by key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Replaces the image `Entrypoint`, and drops the image `Cmd` unless
    /// `cmd` is set too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entrypoint: Vec<String>,
    /// Replaces the image `Cmd`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    /// TCP port whose connect means ready, 1..65535. Unset: the image's
    /// lowest exposed tcp port, else ready on spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_port: Option<u16>,
    /// Grace period between SIGTERM and SIGKILL, at most
    /// [`MAX_SIGTERM_TIMEOUT_SECONDS`]; see [`GuestConfig::sigterm_timeout`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sigterm_timeout_seconds: Option<u64>,
    /// Resolvers for `/etc/resolv.conf`, at most [`MAX_NAMESERVERS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nameservers: Vec<IpAddr>,
    /// The address init gives `eth0`, a /[`IPV4_PREFIX`]. Set with a NIC,
    /// together with `ipv4_gateway`; unset without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv4_address: Option<Ipv4Cidr>,
    /// The default route: the other address of `ipv4_address`'s /31, the
    /// host end of the TAP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv4_gateway: Option<Ipv4Addr>,
}

/// An IPv4 address and prefix length, written `a.b.c.d/n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Cidr {
    pub address: Ipv4Addr,
    pub prefix: u8,
}

impl Ipv4Cidr {
    /// The prefix as a netmask: `/31` is `255.255.255.254`.
    pub fn netmask(&self) -> Ipv4Addr {
        Ipv4Addr::from(
            u32::MAX
                .checked_shl(32 - u32::from(self.prefix))
                .unwrap_or(0),
        )
    }
}

impl fmt::Display for Ipv4Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

impl FromStr for Ipv4Cidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || format!("{s:?} is not an IPv4 address/prefix");
        let (address, prefix) = s.split_once('/').ok_or_else(bad)?;
        // `u8::from_str` takes a leading `+`; a prefix is digits only.
        if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let prefix = prefix.parse().ok().filter(|&p| p <= 32).ok_or_else(bad)?;
        let address = address.parse().map_err(|_| bad())?;
        Ok(Self { address, prefix })
    }
}

impl Serialize for Ipv4Cidr {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Ipv4Cidr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug)]
pub enum ConfigError {
    TooLarge(usize),
    Json(serde_json::Error),
    /// `version` missing, not an integer, or not [`CONFIG_VERSION`].
    Version(Option<Value>),
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge(n) => write!(
                f,
                "guest config is {n} bytes, over the {MAX_CONFIG_BYTES} limit"
            ),
            Self::Json(e) => write!(f, "invalid guest config: {e}"),
            Self::Version(None) => write!(f, "guest config has no `version`"),
            Self::Version(Some(v)) => write!(
                f,
                "unsupported guest config version {v} (this build reads {CONFIG_VERSION})"
            ),
            Self::Invalid(msg) => write!(f, "invalid guest config: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// `GuestConfig` with its version, for writing.
#[derive(Serialize)]
struct Versioned<'a> {
    version: u64,
    #[serde(flatten)]
    config: &'a GuestConfig,
}

impl GuestConfig {
    /// Checks what the schema alone does not: what `execve` and the guest
    /// can carry, and the bounds ADR 0015 sets.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |msg: String| Err(ConfigError::Invalid(msg));
        let nul = |what: &str, s: &str| {
            if s.contains('\0') {
                invalid(format!("{what} {s:?} contains NUL"))
            } else {
                Ok(())
            }
        };
        let names = [
            ("instance_id", Some(&self.instance_id)),
            ("hostname", self.hostname.as_ref()),
        ];
        for (field, name) in names {
            if let Some(name) = name.filter(|n| !is_hostname(n)) {
                return invalid(format!(
                    "`{field}` {name:?}: want 1-{MAX_HOSTNAME} ASCII letters, digits, `-` or `.`"
                ));
            }
            if let Some(name) = name.filter(|n| is_numeric(n)) {
                return invalid(format!(
                    "`{field}` {name:?} reads as an address, not a hostname"
                ));
            }
            if let Some(name) = name.filter(|n| is_loopback_alias(n)) {
                return invalid(format!(
                    "`{field}` {name:?} is a loopback name in /etc/hosts"
                ));
            }
        }
        let too_long = |what: &str, len: usize| {
            if len >= MAX_ARG_STRLEN {
                invalid(format!(
                    "{what} is {len} bytes; execve takes under {MAX_ARG_STRLEN}"
                ))
            } else {
                Ok(())
            }
        };
        let mut exec_bytes = 0;
        for (k, v) in &self.env {
            if k.is_empty() || k.contains('=') {
                return invalid(format!("env key {k:?} is empty or contains `=`"));
            }
            nul("env key", k)?;
            nul("env value", v)?;
            too_long(&format!("env entry {k}"), k.len() + 1 + v.len())?;
            exec_bytes += exec_cost(k.len() + 1 + v.len());
        }
        for arg in self.entrypoint.iter().chain(&self.cmd) {
            nul("argument", arg)?;
            too_long("an argument", arg.len())?;
            exec_bytes += exec_cost(arg.len());
        }
        if exec_bytes > MAX_EXEC_BYTES {
            return invalid(format!(
                "`env`, `entrypoint` and `cmd` take {exec_bytes} bytes for execve, over {MAX_EXEC_BYTES}"
            ));
        }
        if self.readiness_port == Some(0) {
            return invalid("`readiness_port` 0".into());
        }
        if let Some(secs) = self
            .sigterm_timeout_seconds
            .filter(|&s| s > MAX_SIGTERM_TIMEOUT_SECONDS)
        {
            return invalid(format!(
                "`sigterm_timeout_seconds` {secs}, at most {MAX_SIGTERM_TIMEOUT_SECONDS}"
            ));
        }
        if self.nameservers.len() > MAX_NAMESERVERS {
            return invalid(format!(
                "{} `nameservers`, at most {MAX_NAMESERVERS}",
                self.nameservers.len()
            ));
        }
        self.validate_network()
    }

    /// `ipv4_address` and `ipv4_gateway`: both or neither, a /31 of host
    /// addresses, and the gateway its other half.
    fn validate_network(&self) -> Result<(), ConfigError> {
        let invalid = |msg: String| Err(ConfigError::Invalid(msg));
        let (cidr, gateway) = match (self.ipv4_address, self.ipv4_gateway) {
            (None, None) => return Ok(()),
            (Some(cidr), Some(gateway)) => (cidr, gateway),
            _ => return invalid("`ipv4_address` and `ipv4_gateway` go together".into()),
        };
        if cidr.prefix != IPV4_PREFIX {
            return invalid(format!("`ipv4_address` {cidr}: want a /{IPV4_PREFIX}"));
        }
        for (field, addr) in [("ipv4_address", cidr.address), ("ipv4_gateway", gateway)] {
            if addr.is_unspecified()
                || addr.is_loopback()
                || addr.is_multicast()
                || addr.is_broadcast()
            {
                return invalid(format!("`{field}` {addr} is not a host address"));
            }
        }
        let peer = Ipv4Addr::from(u32::from(cidr.address) ^ 1);
        if gateway != peer {
            return invalid(format!(
                "`ipv4_gateway` {gateway}: want {peer}, the other address of {cidr}"
            ));
        }
        Ok(())
    }

    /// The guest's hostname: `hostname`, else `instance_id`.
    pub fn hostname(&self) -> &str {
        self.hostname.as_deref().unwrap_or(&self.instance_id)
    }

    /// The grace period between SIGTERM and SIGKILL.
    pub fn sigterm_timeout(&self) -> Duration {
        Duration::from_secs(
            self.sigterm_timeout_seconds
                .unwrap_or(DEFAULT_SIGTERM_TIMEOUT_SECONDS),
        )
    }

    /// The `guest.json` bytes: compact JSON with `version` first.
    pub fn encode(&self) -> Result<Vec<u8>, ConfigError> {
        self.validate()?;
        let bytes = serde_json::to_vec(&Versioned {
            version: CONFIG_VERSION,
            config: self,
        })
        .map_err(ConfigError::Json)?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge(bytes.len()));
        }
        Ok(bytes)
    }

    /// Parses and validates `guest.json`.
    pub fn decode(bytes: &[u8]) -> Result<Self, ConfigError> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge(bytes.len()));
        }
        let Value::Object(mut map) = serde_json::from_slice(bytes).map_err(ConfigError::Json)?
        else {
            return Err(ConfigError::Invalid("not a JSON object".into()));
        };
        match map.remove("version") {
            Some(v) if v.as_u64() == Some(CONFIG_VERSION) => {}
            other => return Err(ConfigError::Version(other)),
        }
        let config: Self = serde_json::from_value(Value::Object(map)).map_err(ConfigError::Json)?;
        config.validate()?;
        Ok(config)
    }
}

fn is_hostname(name: &str) -> bool {
    (1..=MAX_HOSTNAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// The names init's `/etc/hosts` gives loopback. As the hostname they would
/// resolve there first, so an app binding its own name would be unreachable.
const LOOPBACK_ALIASES: [&str; 3] = ["localhost", "ip6-localhost", "ip6-loopback"];

fn is_loopback_alias(name: &str) -> bool {
    LOOPBACK_ALIASES
        .iter()
        .any(|a| name.eq_ignore_ascii_case(a))
}

/// Whether `getaddrinfo` would read `name` as an IPv4 address: every
/// dot-separated label decimal, octal or `0x` hex, as `inet_aton` takes.
fn is_numeric(name: &str) -> bool {
    name.split('.').all(|label| {
        let hex = label
            .strip_prefix("0x")
            .or_else(|| label.strip_prefix("0X"));
        match hex {
            Some(digits) => digits.bytes().all(|b| b.is_ascii_hexdigit()),
            None => !label.is_empty() && label.bytes().all(|b| b.is_ascii_digit()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> GuestConfig {
        GuestConfig {
            instance_id: "01JD3S2MZ0Q5W4T8E6Y7R9B1C2".into(),
            hostname: Some("web-01jd3s2m".into()),
            env: [("GREETING", "hello \"world\"\n"), ("A", "x=y")]
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .into(),
            entrypoint: vec!["/bin/server".into()],
            cmd: vec!["--listen".into(), "a b".into()],
            readiness_port: Some(8080),
            sigterm_timeout_seconds: Some(3),
            nameservers: vec![
                "1.1.1.1".parse().unwrap(),
                "2606:4700::1111".parse().unwrap(),
            ],
            ipv4_address: Some("10.88.0.3/31".parse().unwrap()),
            ipv4_gateway: Some(Ipv4Addr::new(10, 88, 0, 2)),
        }
    }

    #[test]
    fn round_trips() {
        for config in [
            full(),
            GuestConfig {
                instance_id: "i".into(),
                ..GuestConfig::default()
            },
        ] {
            let bytes = config.encode().unwrap();
            assert_eq!(GuestConfig::decode(&bytes).unwrap(), config);
        }
    }

    #[test]
    fn writes_version_first_and_omits_unset_fields() {
        let config = GuestConfig {
            instance_id: "i".into(),
            ..GuestConfig::default()
        };
        assert_eq!(
            config.encode().unwrap(),
            br#"{"version":1,"instance_id":"i"}"#
        );
    }

    #[test]
    fn checks_the_version_before_the_fields() {
        let cases: [(&str, Option<Value>); 4] = [
            (r#"{"instance_id":"i"}"#, None),
            (
                r#"{"version":2,"instance_id":"i","future":true}"#,
                Some(2.into()),
            ),
            (r#"{"version":"1","instance_id":"i"}"#, Some("1".into())),
            (r#"{"version":1.0,"instance_id":"i"}"#, Some(1.0.into())),
        ];
        for (json, want) in cases {
            match GuestConfig::decode(json.as_bytes()) {
                Err(ConfigError::Version(got)) => assert_eq!(got, want, "{json}"),
                other => panic!("{json}: {other:?}"),
            }
        }
    }

    #[test]
    fn refuses_unknown_fields_and_bad_values() {
        for json in [
            r#"{"version":1,"instance_id":"i","future":true}"#,
            r#"{"version":1}"#,
            r#"{"version":1,"instance_id":""}"#,
            r#"{"version":1,"instance_id":"i","env":{"":"x"}}"#,
            r#"{"version":1,"instance_id":"i","env":{"A=B":"x"}}"#,
            r#"{"version":1,"instance_id":"i","env":{"A":"x\u0000"}}"#,
            r#"{"version":1,"instance_id":"i","cmd":["a\u0000"]}"#,
            r#"{"version":1,"instance_id":"i","readiness_port":0}"#,
            r#"{"version":1,"instance_id":"i","hostname":""}"#,
            r#"{"version":1,"instance_id":"i","hostname":"a b"}"#,
            r#"{"version":1,"instance_id":"i","hostname":"café"}"#,
            r#"{"version":1,"instance_id":"a/b"}"#,
            r#"{"version":1,"instance_id":"i","readiness_port":65536}"#,
            r#"{"version":1,"instance_id":"i","sigterm_timeout_seconds":-1}"#,
            r#"{"version":1,"instance_id":"i","sigterm_timeout_seconds":11}"#,
            r#"{"version":1,"instance_id":"i","nameservers":["example.com"]}"#,
            r#"{"version":1,"instance_id":"i","nameservers":["1.1.1.1","1.0.0.1","9.9.9.9","8.8.8.8"]}"#,
            r#"{"version":1,"instance_id":"i","ipv4_address":"10.88.0.3","ipv4_gateway":"10.88.0.2"}"#,
            r#"{"version":1,"instance_id":"i","ipv4_address":"10.88.0.3/31"}"#,
            "[1]",
            "",
        ] {
            assert!(GuestConfig::decode(json.as_bytes()).is_err(), "{json}");
        }
    }

    #[test]
    fn bounds_the_size() {
        // Control characters take 6 bytes each as JSON escapes, so the
        // file outgrows the cap while the strings fit execve.
        let value = "\u{1}".repeat(100_000);
        let config = GuestConfig {
            instance_id: "i".into(),
            env: [("A", &value), ("B", &value)]
                .map(|(k, v)| (k.to_owned(), v.clone()))
                .into(),
            ..GuestConfig::default()
        };
        assert!(config.validate().is_ok());
        assert!(matches!(config.encode(), Err(ConfigError::TooLarge(_))));
        let bytes = vec![b' '; MAX_CONFIG_BYTES + 1];
        assert!(matches!(
            GuestConfig::decode(&bytes),
            Err(ConfigError::TooLarge(_))
        ));
    }

    #[test]
    fn bounds_the_hostname() {
        let named = |name: String| GuestConfig {
            instance_id: "i".into(),
            hostname: Some(name),
            ..GuestConfig::default()
        };
        assert!(named("a".repeat(MAX_HOSTNAME)).validate().is_ok());
        assert!(named("a".repeat(MAX_HOSTNAME + 1)).validate().is_err());
    }

    #[test]
    fn hostname_defaults_to_the_instance_id() {
        let mut config = GuestConfig {
            instance_id: "01JD3S2MZ0Q5W4T8E6Y7R9B1C2".into(),
            ..GuestConfig::default()
        };
        assert_eq!(config.hostname(), "01JD3S2MZ0Q5W4T8E6Y7R9B1C2");
        config.hostname = Some("web".into());
        assert_eq!(config.hostname(), "web");
    }

    #[test]
    fn bounds_each_execve_string() {
        let config = |env: (&str, usize), arg: usize| GuestConfig {
            instance_id: "i".into(),
            env: [(env.0.to_owned(), "x".repeat(env.1))].into(),
            cmd: vec!["y".repeat(arg)],
            ..GuestConfig::default()
        };
        // `K=` plus the value, then the NUL: one byte under the limit.
        assert!(
            config(("K", MAX_ARG_STRLEN - 3), MAX_ARG_STRLEN - 1)
                .validate()
                .is_ok()
        );
        assert!(config(("K", MAX_ARG_STRLEN - 2), 1).validate().is_err());
        assert!(config(("K", 1), MAX_ARG_STRLEN).validate().is_err());
    }

    #[test]
    fn bounds_the_exec_total() {
        let config = |args: usize| GuestConfig {
            instance_id: "i".into(),
            cmd: vec![String::new(); args],
            ..GuestConfig::default()
        };
        // Each empty argument costs its NUL and its pointer: 9 bytes.
        assert!(config(MAX_EXEC_BYTES / 9).validate().is_ok());
        assert!(config(MAX_EXEC_BYTES / 9 + 1).validate().is_err());
    }

    #[test]
    fn refuses_numeric_hostnames() {
        for name in ["10", "127.0.0.1", "0x7f.1", "0177.0.0.1", "1.2.3.0XfF"] {
            assert!(is_numeric(name), "{name}");
            let config = GuestConfig {
                instance_id: name.into(),
                ..GuestConfig::default()
            };
            assert!(config.validate().is_err(), "{name}");
        }
        for name in ["web-1", "10.web", "0xg", "1..2", "1.2.3.4a"] {
            assert!(!is_numeric(name), "{name}");
        }
    }

    #[test]
    fn refuses_loopback_names() {
        for name in ["localhost", "LocalHost", "ip6-localhost", "ip6-loopback"] {
            let config = GuestConfig {
                instance_id: "i".into(),
                hostname: Some(name.into()),
                ..GuestConfig::default()
            };
            assert!(config.validate().is_err(), "{name}");
        }
        let config = GuestConfig {
            instance_id: "localhost".into(),
            ..GuestConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn parses_and_prints_cidrs() {
        let cidr: Ipv4Cidr = "10.88.0.3/31".parse().unwrap();
        assert_eq!(cidr.address, Ipv4Addr::new(10, 88, 0, 3));
        assert_eq!(cidr.to_string(), "10.88.0.3/31");
        assert_eq!(cidr.netmask(), Ipv4Addr::new(255, 255, 255, 254));
        for (text, mask) in [("0.0.0.0/0", "0.0.0.0"), ("1.2.3.4/32", "255.255.255.255")] {
            let cidr: Ipv4Cidr = text.parse().unwrap();
            assert_eq!(cidr.netmask(), mask.parse::<Ipv4Addr>().unwrap(), "{text}");
        }
        for text in [
            "10.88.0.3",
            "10.88.0.3/",
            "10.88.0.3/33",
            "10.88.0.3/+31",
            "10.88.0/31",
            "::1/31",
            "10.88.0.3/31/1",
        ] {
            assert!(text.parse::<Ipv4Cidr>().is_err(), "{text}");
        }
    }

    #[test]
    fn network_is_a_slash_31_and_its_peer() {
        let net = |address: &str, gateway: &str| GuestConfig {
            instance_id: "i".into(),
            ipv4_address: Some(address.parse().unwrap()),
            ipv4_gateway: Some(gateway.parse().unwrap()),
            ..GuestConfig::default()
        };
        assert!(net("10.88.0.3/31", "10.88.0.2").validate().is_ok());
        assert!(net("10.88.0.2/31", "10.88.0.3").validate().is_ok());
        for (address, gateway) in [
            ("10.88.0.3/30", "10.88.0.2"),
            ("10.88.0.3/32", "10.88.0.2"),
            ("10.88.0.3/24", "10.88.0.1"),
            ("10.88.0.3/31", "10.88.0.4"),
            ("10.88.0.3/31", "10.88.0.3"),
            ("127.0.0.1/31", "127.0.0.0"),
            ("0.0.0.1/31", "0.0.0.0"),
            ("224.0.0.1/31", "224.0.0.0"),
            ("255.255.255.254/31", "255.255.255.255"),
        ] {
            assert!(
                net(address, gateway).validate().is_err(),
                "{address} {gateway}"
            );
        }
        let mut half = net("10.88.0.3/31", "10.88.0.2");
        half.ipv4_gateway = None;
        assert!(half.validate().is_err());
        half.ipv4_gateway = Some(Ipv4Addr::new(10, 88, 0, 2));
        half.ipv4_address = None;
        assert!(half.validate().is_err());
    }

    #[test]
    fn sigterm_timeout_defaults_to_ten_seconds() {
        let mut config = GuestConfig {
            instance_id: "i".into(),
            ..GuestConfig::default()
        };
        assert_eq!(config.sigterm_timeout(), Duration::from_secs(10));
        config.sigterm_timeout_seconds = Some(MAX_SIGTERM_TIMEOUT_SECONDS);
        assert!(config.validate().is_ok());
        assert_eq!(config.sigterm_timeout(), Duration::from_secs(10));
    }

    #[test]
    fn encode_validates() {
        let mut config = full();
        config.env.insert("A=B".into(), String::new());
        assert!(matches!(config.encode(), Err(ConfigError::Invalid(_))));
    }
}
