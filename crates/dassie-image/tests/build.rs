//! Builds a real ext4 with `mkfs.ext4 -d -` and inspects it with `debugfs`.
//!
//! Ignored by default: it needs e2fsprogs 1.47.1 or later with libarchive,
//! which CI's Ubuntu lacks. Run on a Linux box (e.g. the lima VM) with
//! `cargo test -p dassie-image -- --ignored`.

#![cfg(target_os = "linux")]
// `allow-unwrap-in-tests` only covers `#[test]` fns, not the helpers here.
#![allow(clippy::unwrap_used)]

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

use dassie_image::{Digest, Options, Platform, Store, build};
use serde_json::json;
use tar::{Builder, EntryType, Header};

fn header(t: EntryType, mode: u32, uid: u64, size: u64) -> Header {
    let mut h = Header::new_ustar();
    h.set_entry_type(t);
    h.set_mode(mode);
    h.set_uid(uid);
    h.set_gid(uid);
    h.set_mtime(1_700_000_000);
    h.set_size(size);
    h
}

fn put(root: &Path, bytes: &[u8]) -> Digest {
    let d = Digest::of(bytes);
    fs::create_dir_all(root.join("blobs/sha256")).unwrap();
    fs::write(root.join("blobs/sha256").join(d.hex()), bytes).unwrap();
    d
}

/// A one-layer image named `app` whose files are owned by ids the build host
/// almost certainly does not have.
fn layout(root: &Path) {
    let mut layer = Builder::new(Vec::new());
    let mut dir = header(EntryType::Directory, 0o700, 1000, 0);
    layer
        .append_data(&mut dir, "home/app", io::empty())
        .unwrap();
    // A PAX header applies to the entry after it.
    layer
        .append_pax_extensions([("SCHILY.xattr.security.capability", &b"\x01\x02"[..])])
        .unwrap();
    let mut file = header(EntryType::Regular, 0o640, 101, 4);
    layer
        .append_data(&mut file, "srv/data", &b"data"[..])
        .unwrap();
    let mut file = header(EntryType::Regular, 0o4755, 0, 3);
    layer
        .append_data(&mut file, "usr/bin/tool", &b"elf"[..])
        .unwrap();
    let mut link = header(EntryType::Symlink, 0o777, 0, 0);
    layer.append_link(&mut link, "lib", "usr/lib").unwrap();
    let layer = layer.into_inner().unwrap();

    let config = br#"{"config":{"Cmd":["/usr/bin/tool"]}}"#;
    fs::write(root.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "config": { "digest": put(root, config).to_string(), "size": config.len() },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": put(root, &layer).to_string(),
            "size": layer.len(),
        }],
    }))
    .unwrap();
    let index = json!({ "manifests": [{
        "digest": put(root, &manifest).to_string(),
        "size": manifest.len(),
        "annotations": { "org.opencontainers.image.ref.name": "app" },
    }]});
    fs::write(root.join("index.json"), index.to_string()).unwrap();
}

/// The output of one `debugfs -R` request against `image`.
fn debugfs(image: &Path, request: &str) -> String {
    let out = Command::new("debugfs")
        .args(["-R", request])
        .arg(image)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stat(image: &Path, path: &str) -> String {
    debugfs(image, &format!("stat {path}"))
}

#[test]
#[ignore = "needs mkfs.ext4 >= 1.47.1 with libarchive, and debugfs"]
fn builds_ext4_with_ownership_and_reuses_it() {
    let store_dir = tempfile::tempdir().unwrap();
    layout(store_dir.path());
    let store = Store::open(store_dir.path()).unwrap();
    let image = store.image("app", &Platform::host()).unwrap();
    let out = tempfile::tempdir().unwrap();

    let path = build(&store, &image, out.path(), &Options::default()).unwrap();
    let meta = fs::metadata(&path).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o444);

    let home = stat(&path, "/home/app");
    assert!(home.contains("Mode:  0700"), "{home}");
    assert!(home.contains("User:  1000"), "{home}");
    let data = stat(&path, "/srv/data");
    assert!(
        data.contains("User:   101") && data.contains("Group:   101"),
        "{data}"
    );
    // File capabilities survive. (mke2fs 1.47.2 drops `user.*` and
    // `trusted.*` xattrs from tar input; see ADR 0007.)
    let xattrs = debugfs(&path, "ea_list /srv/data");
    assert!(
        xattrs.contains("security.capability (2) = 01 02"),
        "{xattrs}"
    );
    assert!(stat(&path, "/usr/bin/tool").contains("Mode:  04755"));
    assert!(stat(&path, "/lib").contains("Fast link dest: \"usr/lib\""));
    let config = stat(&path, "/.dassie/image.json");
    assert!(config.contains("Size: 36"), "{config}");

    // A second build is a no-op.
    let mtime = meta.modified().unwrap();
    assert_eq!(
        build(&store, &image, out.path(), &Options::default()).unwrap(),
        path
    );
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);

    // The same image builds to the same bytes elsewhere.
    let again = tempfile::tempdir().unwrap();
    let other = build(&store, &image, again.path(), &Options::default()).unwrap();
    assert_eq!(
        Digest::of(&fs::read(&path).unwrap()),
        Digest::of(&fs::read(&other).unwrap())
    );
}
