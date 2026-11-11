//! Dassie host <-> guest protocol.
//!
//! Message types and framing for the vsock channel between `dassie-init` and
//! the host daemon. Must build for the static musl guest init, so the
//! dependency budget is serde plus one small encoding and no async runtime.
//! Framing is generic over `Read + Write` so transports can be swapped in tests.
