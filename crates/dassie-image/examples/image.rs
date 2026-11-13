//! Pulls images and builds guest rootfs artefacts from them, until the
//! daemon exists (M1).
//!
//! ```text
//! image pull  [--store DIR] [--platform OS/ARCH] <ref>
//! image build [--store DIR] [--out DIR] [--platform OS/ARCH] <name|sha256:…>
//! ```
//!
//! `pull` fetches the blobs the store lacks and prints the platform
//! manifest's digest, which is what to pin. The image is also named by the
//! normalized reference (`nginx` becomes `docker.io/library/nginx:latest`).
//!
//! `build` takes that digest or name, or a name given to
//! `skopeo copy docker://… oci:<store>:<name>`. It prints the path of the
//! read-only ext4 artefact, building it only if it does not exist yet. It
//! needs `mkfs.ext4` from e2fsprogs 1.47.1 or later with libarchive.

// A CLI: stdout is its output.
#![allow(clippy::print_stdout)]

use std::path::PathBuf;
use std::process::ExitCode;

use dassie_image::{Client, Options, Platform, Reference, Store, build};

const USAGE: &str = "usage: image pull [--store DIR] [--platform OS/ARCH] <ref>
       image build [--store DIR] [--out DIR] [--platform OS/ARCH] <name|sha256:...>";

fn main() -> ExitCode {
    match run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("image: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let Some((cmd, mut rest)) = args.split_first() else {
        return Err(USAGE.into());
    };
    let mut store = PathBuf::from("/var/lib/dassie/images");
    let mut out = PathBuf::from("/var/lib/dassie/rootfs");
    let mut platform = Platform::host();
    let reference = loop {
        match rest {
            [flag, value, tail @ ..] if flag.starts_with("--") => {
                match flag.as_str() {
                    "--store" => store = value.into(),
                    "--out" if cmd == "build" => out = value.into(),
                    "--platform" => {
                        let (os, arch) = value
                            .split_once('/')
                            .ok_or_else(|| format!("bad --platform {value}"))?;
                        platform = Platform::new(os, arch);
                    }
                    _ => return Err(USAGE.into()),
                }
                rest = tail;
            }
            [reference] => break reference,
            _ => return Err(USAGE.into()),
        }
    };

    match cmd.as_str() {
        "pull" => pull(store, &platform, reference),
        "build" => build_rootfs(store, &out, &platform, reference),
        _ => Err(USAGE.into()),
    }
}

fn pull(store: PathBuf, platform: &Platform, reference: &str) -> Result<(), String> {
    let reference: Reference = reference.parse().map_err(|e| format!("{e}"))?;
    let store = Store::init(store).map_err(|e| e.to_string())?;
    let pulled = Client::new()
        .pull(&store, &reference, platform)
        .map_err(|e| e.to_string())?;
    let total = pulled.blobs_fetched + pulled.blobs_present;
    if pulled.blobs_fetched == 0 {
        eprintln!("all {total} blobs already present, fetched nothing");
    } else {
        #[allow(clippy::cast_precision_loss)]
        let mib = pulled.bytes_fetched as f64 / f64::from(1 << 20);
        eprintln!(
            "fetched {} of {total} blobs ({mib:.1} MiB)",
            pulled.blobs_fetched
        );
    }
    eprintln!("named {}", pulled.name);
    println!("{}", pulled.digest);
    Ok(())
}

fn build_rootfs(
    store: PathBuf,
    out: &std::path::Path,
    platform: &Platform,
    reference: &str,
) -> Result<(), String> {
    let store = Store::open(store).map_err(|e| e.to_string())?;
    let image = store
        .image(reference, platform)
        .map_err(|e| e.to_string())?;
    let built = build(&store, &image, out, &Options::default()).map_err(|e| e.to_string())?;
    println!("{}", built.path.display());
    Ok(())
}
