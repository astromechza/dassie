//! Read side of the blob store: an OCI image layout on disk (ADR 0007).
//!
//! ```text
//! <root>/oci-layout
//! <root>/index.json
//! <root>/blobs/sha256/<hex>
//! ```
//!
//! `skopeo copy docker://… oci:<root>:<name>` writes exactly this, and the
//! registry pull writes the same format through the write side in
//! [`write`](self::write).

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::digest::{Digest, VerifyingReader};
use crate::oci::{self, Descriptor, Index, Manifest, Platform};

mod write;

/// Upper bound for JSON blobs (indexes, manifests, image configs). Real ones
/// are a few KiB; the cap keeps a hostile blob from exhausting memory.
pub(crate) const MAX_JSON: u64 = 4 << 20;

/// Index → index → manifest is the deepest real nesting (a named entry
/// pointing at a multi-platform index).
pub(crate) const MAX_DEPTH: usize = 3;

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, io::Error),
    NotALayout(PathBuf),
    Json(Digest, serde_json::Error),
    TooLarge(PathBuf),
    Tampered(Digest),
    /// A blob being written is not the size its descriptor gives.
    WrongSize(Digest, u64),
    /// Reading a blob's source (e.g. a registry response) failed.
    Read(Digest, io::Error),
    NotFound(String),
    NoPlatform(Digest, Platform),
    /// The image's config says it is for another platform.
    WrongPlatform(Digest, Platform),
    TooDeep(Digest),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, e) => write!(f, "{}: {e}", path.display()),
            Self::NotALayout(root) => {
                write!(
                    f,
                    "{} is not an OCI image layout (no oci-layout)",
                    root.display()
                )
            }
            Self::Json(d, e) => write!(f, "blob {d}: {e}"),
            Self::TooLarge(path) => {
                write!(
                    f,
                    "{}: over {MAX_JSON} bytes, too large for JSON",
                    path.display()
                )
            }
            Self::Tampered(d) => write!(f, "blob {d}: content does not match its digest"),
            Self::WrongSize(d, want) => write!(f, "blob {d}: size is not {want} bytes"),
            Self::Read(d, e) => write!(f, "blob {d}: {e}"),
            Self::NotFound(r) => write!(f, "no image named {r:?} in the store"),
            Self::NoPlatform(d, p) => {
                write!(
                    f,
                    "index {d} has no manifest for {}/{}",
                    p.os, p.architecture
                )
            }
            Self::WrongPlatform(d, p) => {
                write!(f, "image {d} is for {}/{}", p.os, p.architecture)?;
                p.variant.as_ref().map_or(Ok(()), |v| write!(f, "/{v}"))
            }
            Self::TooDeep(d) => write!(f, "index {d} nests too deeply"),
        }
    }
}

impl std::error::Error for Error {}

/// One single-platform image, ready to flatten.
#[derive(Debug, Clone)]
pub struct Image {
    /// The manifest's digest: the image's identity, and the rootfs name.
    pub digest: Digest,
    pub manifest: Manifest,
    /// The OCI config blob, byte for byte (ADR 0005 `/.dassie/image.json`).
    pub config: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// `index.json` entries and nested indexes both deserialize as one of these.
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum Node {
    Index(Index),
    Manifest(Manifest),
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
        let root = root.into();
        if !root.join("oci-layout").is_file() {
            return Err(Error::NotALayout(root));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.root.join("blobs/sha256").join(digest.hex())
    }

    /// Opens a blob for streaming. Reading it to EOF fails if its content
    /// does not match the descriptor's digest.
    pub fn open_blob(&self, desc: &Descriptor) -> Result<VerifyingReader<File>, Error> {
        let path = self.blob_path(&desc.digest);
        let file = File::open(&path).map_err(|e| Error::Io(path, e))?;
        Ok(VerifyingReader::new(file, desc.digest.clone()))
    }

    /// Reads a whole JSON blob and checks its digest.
    pub(crate) fn read_small(&self, digest: &Digest) -> Result<Vec<u8>, Error> {
        let bytes = read_bounded(&self.blob_path(digest))?;
        if Digest::of(&bytes) != *digest {
            return Err(Error::Tampered(digest.clone()));
        }
        Ok(bytes)
    }

    /// Resolves a name (the `ref.name` annotation in `index.json`) or a
    /// `sha256:` digest to one image for `platform`.
    pub fn image(&self, reference: &str, platform: &Platform) -> Result<Image, Error> {
        let mut digest = match reference.parse::<Digest>() {
            Ok(d) => d,
            Err(_) => self.lookup(reference, platform)?,
        };
        for _ in 0..MAX_DEPTH {
            let bytes = self.read_small(&digest)?;
            match serde_json::from_slice(&bytes).map_err(|e| Error::Json(digest.clone(), e))? {
                Node::Index(index) => {
                    digest = index
                        .select(platform)
                        .ok_or_else(|| Error::NoPlatform(digest.clone(), platform.clone()))?
                        .digest
                        .clone();
                }
                Node::Manifest(manifest) => {
                    let config = self.read_small(&manifest.config.digest)?;
                    check_platform(&digest, &config, platform)?;
                    return Ok(Image {
                        digest,
                        manifest,
                        config,
                    });
                }
            }
        }
        Err(Error::TooDeep(digest))
    }

    /// The `index.json` entry named `name`. With several (one per platform),
    /// the one for `platform`.
    fn lookup(&self, name: &str, platform: &Platform) -> Result<Digest, Error> {
        let path = self.root.join("index.json");
        let bytes = read_bounded(&path)?;
        let index: Index =
            serde_json::from_slice(&bytes).map_err(|e| Error::Json(Digest::of(&bytes), e))?;
        let named = Index {
            manifests: index
                .manifests
                .into_iter()
                .filter(|d| d.annotations.get(oci::REF_NAME).is_some_and(|n| n == name))
                .collect(),
        };
        // `skopeo copy` writes one entry per name with no platform; the
        // manifest's config is checked after resolving either way.
        let pick = match named.manifests.as_slice() {
            [only] if only.platform.is_none() => Some(only),
            _ => named.select(platform),
        };
        pick.map(|d| d.digest.clone())
            .ok_or_else(|| Error::NotFound(name.to_owned()))
    }
}

/// Reads a JSON file, refusing anything over [`MAX_JSON`] without reading
/// more than that.
pub(crate) fn read_bounded(path: &Path) -> Result<Vec<u8>, Error> {
    let file = File::open(path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    let mut bytes = Vec::new();
    file.take(MAX_JSON + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::Io(path.to_path_buf(), e))?;
    if bytes.len() as u64 > MAX_JSON {
        return Err(Error::TooLarge(path.to_path_buf()));
    }
    Ok(bytes)
}

/// Rejects an image whose config names a different OS, architecture or (when
/// one is requested) variant, by the same rule as index selection. A config
/// without OS or architecture (only seen in hand-made images) is accepted.
pub(crate) fn check_platform(digest: &Digest, config: &[u8], want: &Platform) -> Result<(), Error> {
    #[derive(Deserialize)]
    struct Fields {
        os: Option<String>,
        architecture: Option<String>,
        variant: Option<String>,
    }
    let got: Fields = serde_json::from_slice(config).map_err(|e| Error::Json(digest.clone(), e))?;
    let got = Platform {
        os: got.os.unwrap_or_else(|| want.os.clone()),
        architecture: got
            .architecture
            .unwrap_or_else(|| want.architecture.clone()),
        variant: got.variant,
    };
    if want.accepts(&got) {
        Ok(())
    } else {
        Err(Error::WrongPlatform(digest.clone(), got))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    /// Writes `bytes` as a blob under `root` and returns its digest.
    pub(crate) fn put(root: &Path, bytes: &[u8]) -> Digest {
        let d = Digest::of(bytes);
        let dir = root.join("blobs/sha256");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(d.hex()), bytes).unwrap();
        d
    }

    /// A layout holding one image named `name`, with the given layer blobs
    /// (uncompressed tars). Returns the manifest digest.
    pub(crate) fn layout(root: &Path, name: &str, config: &[u8], layers: &[Vec<u8>]) -> Digest {
        fs::write(root.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        let config_d = put(root, config);
        let layers: Vec<_> = layers
            .iter()
            .map(|l| {
                json!({
                    "mediaType": "application/vnd.oci.image.layer.v1.tar",
                    "digest": put(root, l).to_string(),
                    "size": l.len(),
                })
            })
            .collect();
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": oci::OCI_MANIFEST,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_d.to_string(),
                "size": config.len(),
            },
            "layers": layers,
        }))
        .unwrap();
        let manifest_d = put(root, &manifest);
        let index = json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": oci::OCI_MANIFEST,
                "digest": manifest_d.to_string(),
                "size": manifest.len(),
                "annotations": { oci::REF_NAME: name },
            }],
        });
        fs::write(root.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
        manifest_d
    }

    #[test]
    fn resolves_name_and_digest() {
        let dir = tempfile::tempdir().unwrap();
        let d = layout(dir.path(), "app", b"{}", &[b"layer".to_vec()]);
        let store = Store::open(dir.path()).unwrap();
        let host = Platform::host();
        let by_name = store.image("app", &host).unwrap();
        assert_eq!(by_name.digest, d);
        assert_eq!(by_name.config, b"{}");
        assert_eq!(store.image(&d.to_string(), &host).unwrap().digest, d);
        assert!(matches!(
            store.image("other", &host),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn follows_nested_index_to_platform() {
        let dir = tempfile::tempdir().unwrap();
        let arm = layout(dir.path(), "x", b"{}", &[]);
        let nested = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "manifests": [
                { "mediaType": oci::OCI_MANIFEST, "digest": Digest::of(b"amd").to_string(), "size": 3,
                  "platform": { "os": "linux", "architecture": "amd64" } },
                { "mediaType": oci::OCI_MANIFEST, "digest": arm.to_string(), "size": 1,
                  "platform": { "os": "linux", "architecture": "arm64" } },
            ],
        }))
        .unwrap();
        let nested_d = put(dir.path(), &nested);
        let store = Store::open(dir.path()).unwrap();
        let img = store
            .image(&nested_d.to_string(), &Platform::new("linux", "arm64"))
            .unwrap();
        assert_eq!(img.digest, arm);
        assert!(matches!(
            store.image(&nested_d.to_string(), &Platform::new("linux", "riscv64")),
            Err(Error::NoPlatform(..))
        ));
    }

    #[test]
    fn checks_the_platform_of_the_resolved_image() {
        let dir = tempfile::tempdir().unwrap();
        let config = br#"{"os":"linux","architecture":"amd64"}"#;
        let d = layout(dir.path(), "app", config, &[]);
        let store = Store::open(dir.path()).unwrap();
        let amd64 = Platform::new("linux", "amd64");
        assert_eq!(store.image("app", &amd64).unwrap().digest, d);
        let arm64 = Platform::new("linux", "arm64");
        assert!(matches!(
            store.image("app", &arm64),
            Err(Error::WrongPlatform(..))
        ));
        assert!(matches!(
            store.image(&d.to_string(), &arm64),
            Err(Error::WrongPlatform(..))
        ));

        let dir = tempfile::tempdir().unwrap();
        let config = br#"{"os":"linux","architecture":"arm","variant":"v6"}"#;
        let d = layout(dir.path(), "app", config, &[]);
        let store = Store::open(dir.path()).unwrap();
        let mut v7 = Platform::new("linux", "arm");
        assert!(store.image(&d.to_string(), &v7).is_ok());
        v7.variant = Some("v7".into());
        assert!(matches!(
            store.image(&d.to_string(), &v7),
            Err(Error::WrongPlatform(..))
        ));
    }

    #[test]
    fn single_named_entry_with_a_platform_must_match() {
        let dir = tempfile::tempdir().unwrap();
        let d = layout(dir.path(), "app", b"{}", &[]);
        let index = json!({ "manifests": [{
            "mediaType": oci::OCI_MANIFEST,
            "digest": d.to_string(),
            "size": 1,
            "platform": { "os": "linux", "architecture": "amd64" },
            "annotations": { oci::REF_NAME: "app" },
        }]});
        fs::write(dir.path().join("index.json"), index.to_string()).unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(store.image("app", &Platform::new("linux", "amd64")).is_ok());
        assert!(matches!(
            store.image("app", &Platform::new("linux", "arm64")),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn oversized_index_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        layout(dir.path(), "app", b"{}", &[]);
        let big = File::create(dir.path().join("index.json")).unwrap();
        big.set_len(MAX_JSON + 1).unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(matches!(
            store.image("app", &Platform::host()),
            Err(Error::TooLarge(_))
        ));
    }

    #[test]
    fn rejects_tampered_blob_and_non_layout() {
        let dir = tempfile::tempdir().unwrap();
        let d = layout(dir.path(), "app", b"{}", &[]);
        let store = Store::open(dir.path()).unwrap();
        fs::write(store.blob_path(&d), b"{\"config\":1}").unwrap();
        assert!(matches!(
            store.image("app", &Platform::host()),
            Err(Error::Tampered(_))
        ));
        assert!(matches!(
            Store::open(dir.path().join("blobs")),
            Err(Error::NotALayout(_))
        ));
    }
}
