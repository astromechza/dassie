//! Write side of the blob store, used by the registry pull (ADR 0007).
//!
//! Every write goes to a temporary file in the destination's directory and is
//! renamed into place, so a crash or a failed check never leaves a partial
//! blob or `index.json` at its final name.

use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use super::{Error, Store, read_bounded};
use crate::atomic;
use crate::digest::Digest;
use crate::oci::{self, Descriptor};

const OCI_LAYOUT: &str = r#"{"imageLayoutVersion":"1.0.0"}"#;
const EMPTY_INDEX: &str = r#"{"schemaVersion":2,"manifests":[]}"#;

/// `index.json` as written by others: only `manifests` is interpreted, and
/// every other field is kept as it was.
#[derive(Serialize, Deserialize)]
struct RawIndex {
    #[serde(flatten)]
    rest: Map<String, Value>,
    manifests: Vec<Value>,
}

impl Store {
    /// Opens the layout at `root`, creating an empty one first if there is
    /// none. An existing layout (e.g. one `skopeo` wrote) is left as it is.
    pub fn init(root: impl Into<PathBuf>) -> Result<Self, Error> {
        let root = root.into();
        let blobs = root.join("blobs/sha256");
        fs::create_dir_all(&blobs).map_err(|e| Error::Io(blobs, e))?;
        write_if_absent(&root.join("oci-layout"), OCI_LAYOUT)?;
        write_if_absent(&root.join("index.json"), EMPTY_INDEX)?;
        Self::open(root)
    }

    /// Whether the blob is in the store. Its content is checked when read.
    pub fn has_blob(&self, digest: &Digest) -> bool {
        self.blob_path(digest).is_file()
    }

    /// Removes a blob, e.g. one found corrupt. Absent is not an error.
    pub(crate) fn remove_blob(&self, digest: &Digest) -> Result<(), Error> {
        let path = self.blob_path(digest);
        match fs::remove_file(&path) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(Error::Io(path, e)),
            _ => Ok(()),
        }
    }

    /// Streams `src` into the store as blob `digest`, which must be exactly
    /// `size` bytes. Returns `false`, without reading `src`, if the blob is
    /// already present.
    ///
    /// The bytes are hashed on the way to a temporary file, and only renamed
    /// into place once the size and digest match. On any error, including a
    /// failing `src`, nothing is left behind. At most `size + 1` bytes are
    /// read, so an endless source cannot fill the disk.
    pub fn put(&self, digest: &Digest, size: u64, src: impl Read) -> Result<bool, Error> {
        let path = self.blob_path(digest);
        if path.is_file() {
            return Ok(false);
        }
        atomic::write_with(&path, Error::Io, |tmp| {
            let io = |e| Error::Io(tmp.to_path_buf(), e);
            let mut file = File::create_new(tmp).map_err(io)?;
            let mut src = src.take(size.saturating_add(1));
            let mut hasher = Sha256::new();
            let mut buf = vec![0; 64 << 10];
            let mut total = 0u64;
            loop {
                let n = match src.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => return Err(Error::Read(digest.clone(), e)),
                };
                let chunk = buf.get(..n).unwrap_or_default();
                hasher.update(chunk);
                file.write_all(chunk).map_err(io)?;
                total += n as u64;
            }
            if total != size {
                return Err(Error::WrongSize(digest.clone(), size));
            }
            if Digest::finish(hasher) != *digest {
                return Err(Error::Tampered(digest.clone()));
            }
            Ok(true)
        })
    }

    /// Names `desc` (a manifest already in the store) `name` in
    /// `index.json`, replacing any entry of that name. The entry carries the
    /// descriptor's media type, digest and size plus the `ref.name`
    /// annotation, as `skopeo copy … oci:<root>:<name>` writes it. Other
    /// entries and fields are kept byte-for-byte in meaning.
    ///
    /// The file is replaced atomically. Two concurrent calls can still lose
    /// one update (read-modify-write); callers serialize them.
    pub fn set_ref(&self, name: &str, desc: &Descriptor) -> Result<(), Error> {
        let path = self.root.join("index.json");
        let bytes = read_bounded(&path)?;
        let mut index: RawIndex =
            serde_json::from_slice(&bytes).map_err(|e| Error::Json(Digest::of(&bytes), e))?;
        let pointer = format!("/annotations/{}", oci::REF_NAME);
        index
            .manifests
            .retain(|m| m.pointer(&pointer).and_then(Value::as_str) != Some(name));
        index.manifests.push(json!({
            "mediaType": desc.media_type,
            "digest": desc.digest.to_string(),
            "size": desc.size,
            "annotations": { oci::REF_NAME: name },
        }));
        let out = serde_json::to_vec(&index).map_err(|e| Error::Json(Digest::of(&bytes), e))?;
        atomic::write_with(&path, Error::Io, |tmp| {
            fs::write(tmp, &out).map_err(|e| Error::Io(tmp.to_path_buf(), e))
        })
    }
}

fn write_if_absent(path: &Path, content: &str) -> Result<(), Error> {
    if path.exists() {
        return Ok(());
    }
    atomic::write_with(path, Error::Io, |tmp| {
        fs::write(tmp, content).map_err(|e| Error::Io(tmp.to_path_buf(), e))
    })
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::Platform;

    /// Everything under `dir` except the given names, to spot leftovers.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Yields some bytes, then fails, like a connection dropped mid-body.
    struct Broken(usize);

    impl Read for Broken {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::new(ErrorKind::ConnectionReset, "reset"));
            }
            let n = self.0.min(buf.len());
            buf[..n].fill(b'x');
            self.0 -= n;
            Ok(n)
        }
    }

    #[test]
    fn init_creates_an_empty_layout_and_keeps_an_existing_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::init(dir.path()).unwrap();
        assert_eq!(entries(dir.path()), ["blobs", "index.json", "oci-layout"]);
        assert!(matches!(
            store.image("app", &Platform::host()),
            Err(Error::NotFound(_))
        ));

        let d = super::super::tests::layout(dir.path(), "app", b"{}", &[]);
        let store = Store::init(dir.path()).unwrap();
        assert_eq!(store.image("app", &Platform::host()).unwrap().digest, d);
    }

    #[test]
    fn put_verifies_and_skips_present_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::init(dir.path()).unwrap();
        let d = Digest::of(b"layer");
        assert!(store.put(&d, 5, &b"layer"[..]).unwrap());
        assert_eq!(fs::read(store.blob_path(&d)).unwrap(), b"layer");
        // Present: the source is not read at all.
        assert!(!store.put(&d, 5, Broken(0)).unwrap());
    }

    #[test]
    fn put_leaves_nothing_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::init(dir.path()).unwrap();
        let blobs = dir.path().join("blobs/sha256");
        let d = Digest::of(b"layer");
        assert!(matches!(
            store.put(&d, 5, &b"lay3r"[..]),
            Err(Error::Tampered(_))
        ));
        assert!(matches!(
            store.put(&d, 5, &b"layer!"[..]),
            Err(Error::WrongSize(_, 5))
        ));
        assert!(matches!(
            store.put(&d, 5, &b"lay"[..]),
            Err(Error::WrongSize(_, 5))
        ));
        assert!(matches!(
            store.put(&d, 1 << 20, Broken(100_000)),
            Err(Error::Read(_, _))
        ));
        assert_eq!(entries(&blobs), Vec::<String>::new());
    }

    #[test]
    fn set_ref_replaces_by_name_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::init(dir.path()).unwrap();
        let path = dir.path().join("index.json");
        fs::write(
            &path,
            r#"{"schemaVersion":2,"extra":1,"manifests":[
                {"mediaType":"m","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":1,
                 "annotations":{"org.opencontainers.image.ref.name":"other"},"future":true}]}"#,
        )
        .unwrap();
        let desc = |bytes: &[u8]| Descriptor {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            digest: Digest::of(bytes),
            size: bytes.len() as u64,
            platform: None,
            annotations: std::collections::BTreeMap::new(),
        };
        store.set_ref("app", &desc(b"one")).unwrap();
        store.set_ref("app", &desc(b"two")).unwrap();

        let index: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(index["extra"], 1);
        let manifests = index["manifests"].as_array().unwrap();
        assert_eq!(manifests.len(), 2);
        assert_eq!(manifests[0]["future"], true);
        assert_eq!(manifests[1]["digest"], Digest::of(b"two").to_string());
        assert_eq!(manifests[1]["annotations"][oci::REF_NAME], "app");
        assert_eq!(entries(dir.path()), ["blobs", "index.json", "oci-layout"]);
    }
}
