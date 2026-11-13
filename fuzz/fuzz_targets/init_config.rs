//! Feeds `dassie-init`'s config loading (ADR 0005) the four inputs it reads
//! at boot: the image config JSON, the kernel cmdline, `/etc/passwd` and
//! `/etc/group`, split on NUL bytes. The image config comes from whatever
//! image was pulled, and this runs in PID 1, where a panic is a kernel panic
//! and a crash loop. So nothing may panic, and an accepted spec must hold:
//!
//! - a non-empty argv and no readiness port 0;
//! - every env key exactly once, with `PATH` and `HOME` set;
//! - at most 3 `dassie.dns` resolvers, each written as exactly one
//!   `nameserver` line of `/etc/resolv.conf`, in order.

#![no_main]

use std::collections::HashSet;

use dassie_init::boot::RootSpec;
use dassie_init::config::{self, resolv_conf};
use dassie_init::user::Accounts;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut parts = data.splitn(4, |&b| b == 0).map(std::str::from_utf8);
    let mut next = || parts.next().unwrap_or(Ok("")).ok();
    let (Some(image), Some(cmdline), Some(passwd), Some(group)) = (next(), next(), next(), next())
    else {
        return;
    };

    let _ = RootSpec::from_cmdline(cmdline);
    let spec = match config::build(image, cmdline, &Accounts::parse(passwd, group)) {
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

    assert!(spec.dns.len() <= 3, "{} resolvers", spec.dns.len());
    let written: Vec<String> = resolv_conf(&spec.dns)
        .lines()
        .filter_map(|l| l.strip_prefix("nameserver ").map(str::to_owned))
        .collect();
    let want: Vec<String> = spec.dns.iter().map(ToString::to_string).collect();
    assert_eq!(written, want, "resolv.conf does not match dassie.dns");
});
