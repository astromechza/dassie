//! Builds a real erofs with `mkfs.erofs --tar=f` and inspects it with
//! `dump.erofs`.
//!
//! Ignored by default: it needs erofs-utils 1.8 or later, which CI's Ubuntu
//! lacks. Run on a Linux box (e.g. the lima VM) with
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

/// What `dump.erofs` prints for `args` against `image`.
fn dump(image: &Path, args: &[&str]) -> String {
    let out = Command::new("dump.erofs")
        .args(args)
        .arg(image)
        .output()
        .unwrap();
    assert!(out.status.success(), "dump.erofs {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stat(image: &Path, path: &str) -> String {
    dump(image, &[&format!("--path={path}")])
}

#[test]
#[ignore = "needs mkfs.erofs and dump.erofs from erofs-utils >= 1.8"]
fn builds_erofs_with_ownership_and_reuses_it() {
    let store_dir = tempfile::tempdir().unwrap();
    layout(store_dir.path());
    let store = Store::open(store_dir.path()).unwrap();
    let image = store.image("app", &Platform::host()).unwrap();
    let out = tempfile::tempdir().unwrap();

    let path = build(&store, &image, out.path(), &Options::default())
        .unwrap()
        .path;
    assert!(path.to_string_lossy().ends_with(".v3.erofs"), "{path:?}");
    let meta = fs::metadata(&path).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o444);

    // The flattener's root, not mkfs.erofs's default `0777`.
    let root = stat(&path, "/");
    assert!(root.contains("Uid: 0   Gid: 0  Access: 0755"), "{root}");
    let home = stat(&path, "/home/app");
    assert!(
        home.contains("Uid: 1000   Gid: 1000  Access: 0700"),
        "{home}"
    );
    let data = stat(&path, "/srv/data");
    assert!(data.contains("Uid: 101   Gid: 101  Access: 0640"), "{data}");
    // File capabilities survive (dump.erofs shows only their size).
    assert!(!data.contains("Xattr size: 0"), "{data}");
    assert_eq!(dump(&path, &["--path=/srv/data", "--cat"]), "data");
    // dump.erofs prints only the permission bits; the setuid bit is kept
    // (checked by mounting it, which needs root).
    let tool = stat(&path, "/usr/bin/tool");
    assert!(tool.contains("Access: 0755"), "{tool}");
    assert!(stat(&path, "/lib").contains("symlink"));
    let config = stat(&path, "/.dassie/image.json");
    assert!(config.contains("Size: 36 "), "{config}");

    // A second build is a no-op.
    let mtime = meta.modified().unwrap();
    assert_eq!(
        build(&store, &image, out.path(), &Options::default())
            .unwrap()
            .path,
        path
    );
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);

    // The same image builds to the same bytes elsewhere.
    let again = tempfile::tempdir().unwrap();
    let other = build(&store, &image, again.path(), &Options::default())
        .unwrap()
        .path;
    assert_eq!(
        Digest::of(&fs::read(&path).unwrap()),
        Digest::of(&fs::read(&other).unwrap())
    );
}
