//! OCI image to erofs guest root filesystem builder (ADR 0007, ADR 0009).
//!
//! - [`Store`] is an OCI image layout on disk: the content-addressed blob
//!   store that doubles as the layer cache.
//! - [`Client`] pulls an image from a registry into the store, fetching only
//!   the blobs it lacks and verifying every digest.
//! - [`Tree`] flattens an image's layers into one tar stream, applying
//!   whiteouts and keeping numeric ownership, modes, device nodes and xattrs.
//! - [`build`] pipes that stream into `mkfs.erofs --tar=f` to produce one
//!   immutable, read-only, digest-named erofs per image.
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

mod atomic;
mod build;
mod digest;
mod flatten;
mod oci;
mod reference;
mod registry;
mod store;

pub use build::{BuildOutput, EXTENSION, FORMAT_VERSION, Options, artefact_path, build};
pub use digest::{Digest, ParseError, VerifyingReader};
pub use flatten::Tree;
pub use oci::{Compression, Descriptor, Index, Manifest, Platform};
pub use reference::{Reference, ReferenceError};
pub use registry::{Client, Event, MAX_LAYERS, Pulled, Retry};
pub use store::{Image, Store};

/// Errors from each stage.
pub mod error {
    pub use crate::build::Error as Build;
    pub use crate::flatten::Error as Flatten;
    pub use crate::registry::Error as Registry;
    pub use crate::store::Error as Store;
}
