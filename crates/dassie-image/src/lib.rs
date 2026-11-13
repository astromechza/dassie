//! OCI image to ext4 guest root filesystem builder (ADR 0007).
//!
//! - [`Store`] reads an OCI image layout on disk: the content-addressed blob
//!   store that doubles as the layer cache.
//! - [`Tree`] flattens an image's layers into one tar stream, applying
//!   whiteouts and keeping numeric ownership, modes, device nodes and xattrs.
//! - [`build`] pipes that stream into `mkfs.ext4 -d -` to produce one
//!   immutable, read-only, digest-named ext4 per image.
//!
//! Layers are never unpacked to a host directory, which would lose ownership
//! without root (and everything else on macOS).

// Parses image layers, which are untrusted input: no panicking paths.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod build;
mod digest;
mod flatten;
mod oci;
mod store;

pub use build::{BuildOutput, FORMAT_VERSION, Options, artefact_path, build};
pub use digest::{Digest, ParseError, VerifyingReader};
pub use flatten::{Stats, Tree};
pub use oci::{Compression, Descriptor, Index, Manifest, Platform};
pub use store::{Image, Store};

/// Errors from each stage.
pub mod error {
    pub use crate::build::Error as Build;
    pub use crate::flatten::Error as Flatten;
    pub use crate::store::Error as Store;
}
