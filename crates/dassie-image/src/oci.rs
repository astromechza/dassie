//! The parts of the OCI image spec dassie reads: indexes, manifests and
//! descriptors. Docker v2 layer media types are accepted as equivalents,
//! since registries and `skopeo` still produce them. Indexes and manifests
//! are told apart by shape, not media type (see `store`).

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::Digest;

pub(crate) const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// Annotation `skopeo copy … oci:DIR:NAME` sets on the `index.json` entry.
pub(crate) const REF_NAME: &str = "org.opencontainers.image.ref.name";

/// How a layer blob is compressed, from its media type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
}

impl Compression {
    /// `None` for media types dassie cannot unpack (zstd, foreign layers).
    pub fn of_layer(media_type: &str) -> Option<Self> {
        match media_type {
            "application/vnd.oci.image.layer.v1.tar"
            | "application/vnd.docker.image.rootfs.diff.tar" => Some(Self::None),
            "application/vnd.oci.image.layer.v1.tar+gzip"
            | "application/vnd.docker.image.rootfs.diff.tar.gzip" => Some(Self::Gzip),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    #[serde(default)]
    pub media_type: String,
    pub digest: Digest,
    pub size: u64,
    #[serde(default)]
    pub platform: Option<Platform>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    #[serde(default)]
    pub variant: Option<String>,
}

impl Platform {
    pub fn new(os: &str, architecture: &str) -> Self {
        Self {
            os: os.to_owned(),
            architecture: architecture.to_owned(),
            variant: None,
        }
    }

    /// The platform of this host, in OCI names.
    pub fn host() -> Self {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        Self::new("linux", arch)
    }

    /// Whether a descriptor's platform satisfies this one. A requested
    /// variant must match; an unrequested one is ignored.
    pub(crate) fn accepts(&self, other: &Self) -> bool {
        self.os == other.os
            && self.architecture == other.architecture
            && (self.variant.is_none() || self.variant == other.variant)
    }
}

/// An image index (`index.json`, or a multi-platform manifest list).
#[derive(Debug, Clone, Deserialize)]
pub struct Index {
    pub manifests: Vec<Descriptor>,
}

impl Index {
    /// The first manifest for `platform`. Entries without a platform are not
    /// picked: in a multi-platform index they are attestations and the like.
    pub fn select(&self, platform: &Platform) -> Option<&Descriptor> {
        self.manifests
            .iter()
            .find(|d| d.platform.as_ref().is_some_and(|p| platform.accepts(p)))
    }
}

/// A single-platform image manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// Optional in OCI manifests, always set in Docker v2 ones.
    #[serde(default)]
    pub media_type: String,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(arch: &str, variant: Option<&str>) -> Descriptor {
        Descriptor {
            media_type: OCI_MANIFEST.into(),
            digest: Digest::of(arch.as_bytes()),
            size: 1,
            platform: Some(Platform {
                os: "linux".into(),
                architecture: arch.into(),
                variant: variant.map(Into::into),
            }),
            annotations: BTreeMap::new(),
        }
    }

    #[test]
    fn selects_platform_and_variant() {
        let index = Index {
            manifests: vec![
                desc("amd64", None),
                desc("arm", Some("v7")),
                desc("arm64", Some("v8")),
            ],
        };
        let pick = |p: Platform| index.select(&p).map(|d| d.digest.clone());
        assert_eq!(
            pick(Platform::new("linux", "arm64")),
            Some(Digest::of(b"arm64"))
        );
        let mut v6 = Platform::new("linux", "arm");
        v6.variant = Some("v6".into());
        assert_eq!(pick(v6), None);
        assert_eq!(pick(Platform::new("linux", "s390x")), None);
    }

    #[test]
    fn layer_compression() {
        assert_eq!(
            Compression::of_layer("application/vnd.docker.image.rootfs.diff.tar.gzip"),
            Some(Compression::Gzip)
        );
        assert_eq!(
            Compression::of_layer("application/vnd.oci.image.layer.v1.tar+zstd"),
            None
        );
    }
}
