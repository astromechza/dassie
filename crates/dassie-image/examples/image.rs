//! Builds a guest rootfs from an image in a local OCI layout, until the
//! daemon exists (M1).
//!
//! ```text
//! image build [--store DIR] [--out DIR] [--platform OS/ARCH] <name|sha256:…>
//! ```
//!
//! `<name>` is the name given to `skopeo copy docker://… oci:<store>:<name>`.
//! Prints the path of the read-only ext4 artefact, building it only if it
//! does not exist yet. Needs `mkfs.ext4` from e2fsprogs 1.47.1 or later with
//! libarchive.

// A CLI: stdout is its output.
#![allow(clippy::print_stdout)]

use std::path::PathBuf;
use std::process::ExitCode;

use dassie_image::{Options, Platform, Store, build};

const USAGE: &str =
    "usage: image build [--store DIR] [--out DIR] [--platform OS/ARCH] <name|sha256:...>";

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
    let Some(("build", mut rest)) = args.split_first().map(|(c, r)| (c.as_str(), r)) else {
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
                    "--out" => out = value.into(),
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

    let store = Store::open(store).map_err(|e| e.to_string())?;
    let image = store
        .image(reference, &platform)
        .map_err(|e| e.to_string())?;
    let built = build(&store, &image, &out, &Options::default()).map_err(|e| e.to_string())?;
    println!("{}", built.path.display());
    Ok(())
}
