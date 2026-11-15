//! Feeds `dassie-init`'s config loading (ADR 0005, ADR 0015) the five inputs
//! it reads at boot: the image config JSON, `guest.json`, the kernel cmdline,
//! `/etc/passwd` and `/etc/group`, split on NUL bytes. The image config comes
//! from whatever image was pulled, and this runs in PID 1, where a panic is a
//! kernel panic and a crash loop. So nothing may panic, and:
//!
//! - an accepted `guest.json` re-encodes to a config that decodes the same;
//! - an accepted spec has a non-empty argv and no readiness port 0, every
//!   env key exactly once, `PATH` and `HOME` set, and `DASSIE_INSTANCE_ID`
//!   from the guest config;
//! - `/etc/hosts` ends with the hostname's line, and `/etc/resolv.conf` has
//!   exactly one `nameserver` line per resolver, in order.

#![no_main]

use std::collections::HashSet;

use dassie_init::boot::Cmdline;
use dassie_init::config::{self, NO_NIC_ADDRESS, hosts, resolv_conf};
use dassie_init::user::Accounts;
use dassie_proto::GuestConfig;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut parts = data.splitn(5, |&b| b == 0);
    let mut next = || parts.next().unwrap_or_default();
    let (image, guest, cmdline, passwd, group) = (next(), next(), next(), next(), next());
    let (Ok(image), Ok(cmdline), Ok(passwd), Ok(group)) = (
        std::str::from_utf8(image),
        std::str::from_utf8(cmdline),
        std::str::from_utf8(passwd),
        std::str::from_utf8(group),
    ) else {
        return;
    };

    let address = match Cmdline::parse(cmdline) {
        Ok(cmdline) => cmdline.address.unwrap_or(NO_NIC_ADDRESS),
        Err(e) => {
            let _ = e.to_string();
            NO_NIC_ADDRESS
        }
    };
    let guest = match GuestConfig::decode(guest) {
        Ok(guest) => guest,
        Err(e) => {
            let _ = e.to_string();
            return;
        }
    };
    let encoded = guest.encode().expect("a decoded config re-encodes");
    assert_eq!(
        GuestConfig::decode(&encoded).expect("a re-encoded config decodes"),
        guest,
        "round trip changed the config"
    );

    let hostname = guest.hostname();
    let etc_hosts = hosts(hostname, address);
    assert_eq!(
        etc_hosts.lines().last(),
        Some(format!("{address}\t{hostname}").as_str()),
        "hostname not mapped on the last line"
    );
    let written: Vec<String> = resolv_conf(&guest.nameservers)
        .lines()
        .filter_map(|l| l.strip_prefix("nameserver ").map(str::to_owned))
        .collect();
    let want: Vec<String> = guest.nameservers.iter().map(ToString::to_string).collect();
    assert_eq!(written, want, "resolv.conf does not match nameservers");

    let spec = match config::build(image, &guest, address, &Accounts::parse(passwd, group)) {
        Ok(spec) => spec,
        Err(e) => {
            let _ = e.to_string();
            return;
        }
    };

    assert!(!spec.argv.is_empty(), "empty argv");
    assert_ne!(spec.ready_port, Some(0), "readiness port 0");

    let mut keys = HashSet::new();
    for (k, _) in &spec.env {
        assert!(keys.insert(k.as_str()), "env key {k:?} twice");
    }
    assert!(keys.contains("PATH") && keys.contains("HOME"), "no PATH or HOME");
    assert!(
        spec.env
            .iter()
            .any(|(k, v)| k == "DASSIE_INSTANCE_ID" && *v == guest.instance_id),
        "DASSIE_INSTANCE_ID is not the instance id"
    );
});
