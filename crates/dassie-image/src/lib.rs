//! OCI image to ext4 guest root filesystem builder.
//!
//! Rootfs assembly must preserve ownership, device nodes and xattrs, so it must
//! not round-trip layers through a host (e.g. macOS) directory.
