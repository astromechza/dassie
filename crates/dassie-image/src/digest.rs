//! Content digests (`sha256:<hex>`) and a reader that verifies one.

use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

use serde::{Deserialize, Deserializer};
use sha2::{Digest as _, Sha256};

/// A validated `sha256:<64 lowercase hex>` digest.
///
/// Validation matters beyond correctness: the hex part becomes a file name in
/// the blob store, so it must never contain a path separator.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    hex: String,
}

/// The only algorithm accepted.
const PREFIX: &str = "sha256:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid digest {:?}: want sha256:<64 hex>", self.0)
    }
}

impl std::error::Error for ParseError {}

impl Digest {
    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            hex: hex(&Sha256::digest(bytes)),
        }
    }

    /// The 64 lowercase hex characters, without the algorithm prefix.
    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// A version 4-shaped UUID taken from the first 16 bytes of the digest, so
    /// a filesystem built from this digest always gets the same UUID.
    pub fn uuid(&self) -> String {
        let h = &self.hex;
        // `hex` is 64 ASCII characters, so every range below is in bounds.
        let part = |from: usize, to: usize| h.get(from..to).unwrap_or_default();
        format!(
            "{}-{}-4{}-8{}-{}",
            part(0, 8),
            part(8, 12),
            part(13, 16),
            part(17, 20),
            part(20, 32)
        )
    }
}

impl FromStr for Digest {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s
            .strip_prefix(PREFIX)
            .filter(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .ok_or_else(|| ParseError(s.to_owned()))?;
        Ok(Self {
            hex: hex.to_owned(),
        })
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}{}", self.hex)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Wraps a reader and fails the read that reaches EOF if the bytes seen do
/// not hash to the expected digest. Callers must read to EOF for the check to
/// run.
#[derive(Debug)]
pub struct VerifyingReader<R> {
    inner: R,
    hasher: Sha256,
    expected: Digest,
    checked: bool,
}

impl<R: Read> VerifyingReader<R> {
    pub fn new(inner: R, expected: Digest) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            expected,
            checked: false,
        }
    }
}

impl<R: Read> Read for VerifyingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 && !buf.is_empty() && !self.checked {
            self.checked = true;
            let actual = Digest {
                hex: hex(&self.hasher.clone().finalize()),
            };
            if actual != self.expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("blob is {actual}, want {}", self.expected),
                ));
            }
        }
        self.hasher.update(buf.get(..n).unwrap_or_default());
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_sha256_hex() {
        let d = Digest::of(b"hi");
        assert_eq!(d.to_string().parse::<Digest>(), Ok(d.clone()));
        for bad in [
            "sha512:ab",
            "sha256:../../etc/passwd",
            &d.to_string().to_uppercase(),
            &format!("{d}0"),
            d.hex(),
        ] {
            assert!(bad.parse::<Digest>().is_err(), "{bad}");
        }
    }

    #[test]
    fn uuid_is_stable_and_shaped() {
        let d = Digest::of(b"hi");
        let u = d.uuid();
        assert_eq!(u, Digest::of(b"hi").uuid());
        let parts: Vec<_> = u.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12]);
    }

    #[test]
    fn verifying_reader_checks_at_eof() {
        let mut ok = VerifyingReader::new(&b"layer"[..], Digest::of(b"layer"));
        let mut out = Vec::new();
        ok.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"layer");

        let mut bad = VerifyingReader::new(&b"tampered"[..], Digest::of(b"layer"));
        let err = bad.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
