//! Image references: `[registry/]repository[:tag][@sha256:…]`.

use std::fmt;
use std::str::FromStr;

use crate::Digest;

/// The registry a reference names when its first component is not a host.
const DOCKER_HUB: &str = "docker.io";

/// A parsed, normalized image reference, following Docker's rules:
///
/// - the first component is a registry only if it contains `.` or `:` or is
///   `localhost`; otherwise the registry is `docker.io`;
/// - on `docker.io`, a one-component repository gets `library/`;
/// - without a tag or digest, the tag is `latest`;
/// - with both, the digest decides what is pulled and the tag is kept only
///   for the name.
///
/// Every part is checked against a strict character set, since each one
/// ends up in a URL path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    registry: String,
    repository: String,
    tag: Option<String>,
    digest: Option<Digest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceError {
    input: String,
    reason: &'static str,
}

impl fmt::Display for ReferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid image reference {:?}: {}",
            self.input, self.reason
        )
    }
}

impl std::error::Error for ReferenceError {}

impl Reference {
    /// The registry host, with any port, e.g. `docker.io` or `ghcr.io`.
    pub fn registry(&self) -> &str {
        &self.registry
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    pub fn digest(&self) -> Option<&Digest> {
        self.digest.as_ref()
    }

    /// The host serving the registry API. Docker Hub's is not `docker.io`.
    pub(crate) fn api_host(&self) -> &str {
        if self.registry == DOCKER_HUB {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }

    /// What to ask the registry for: the digest if pinned, else the tag.
    pub(crate) fn target(&self) -> String {
        match (&self.digest, &self.tag) {
            (Some(d), _) => d.to_string(),
            (None, Some(t)) => t.clone(),
            (None, None) => "latest".to_owned(),
        }
    }
}

impl FromStr for Reference {
    type Err = ReferenceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = |reason| ReferenceError {
            input: s.to_owned(),
            reason,
        };
        let (rest, digest) = match s.split_once('@') {
            Some((rest, d)) => (rest, Some(d.parse().map_err(|_| err("bad digest"))?)),
            None => (s, None),
        };
        let (registry, path) = match rest.split_once('/') {
            Some((first, path)) if first.contains(['.', ':']) || first == "localhost" => {
                (first, path)
            }
            _ => (DOCKER_HUB, rest),
        };
        let registry = match registry {
            "index.docker.io" | "registry-1.docker.io" => DOCKER_HUB,
            r => r,
        };
        let (path, tag) = match path.split_once(':') {
            Some((p, t)) => (p, Some(t)),
            None => (path, None),
        };

        if registry.is_empty()
            || !registry
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
        {
            return Err(err("bad registry"));
        }
        if path.len() > 255 || !path.split('/').all(valid_component) {
            return Err(err("bad repository"));
        }
        if let Some(t) = tag
            && !valid_tag(t)
        {
            return Err(err("bad tag"));
        }

        let repository = if registry == DOCKER_HUB && !path.contains('/') {
            format!("library/{path}")
        } else {
            path.to_owned()
        };
        let tag = match (tag, &digest) {
            (None, None) => Some("latest".to_owned()),
            (t, _) => t.map(str::to_owned),
        };
        Ok(Self {
            registry: registry.to_owned(),
            repository,
            tag,
            digest,
        })
    }
}

/// A repository path component: lowercase alphanumerics, with `.`, `_` and
/// `-` only between them (so never `.` or `..`).
fn valid_component(c: &str) -> bool {
    let edge = |b: Option<u8>| b.is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    edge(c.bytes().next())
        && edge(c.bytes().last())
        && c.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// A tag: up to 128 of `[A-Za-z0-9_.-]`, not starting with `.` or `-`.
fn valid_tag(t: &str) -> bool {
    t.len() <= 128
        && t.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// The normalized form: `registry/repository[:tag][@digest]`. This is the
/// name a pull records in `index.json`.
impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_docker() {
        let d = Digest::of(b"m");
        for (input, want) in [
            ("nginx", "docker.io/library/nginx:latest"),
            ("nginx:alpine", "docker.io/library/nginx:alpine"),
            (
                "nginxinc/nginx-unprivileged:alpine",
                "docker.io/nginxinc/nginx-unprivileged:alpine",
            ),
            (
                "index.docker.io/library/nginx",
                "docker.io/library/nginx:latest",
            ),
            ("ghcr.io/owner/app:v1.2", "ghcr.io/owner/app:v1.2"),
            ("localhost/app", "localhost/app:latest"),
            ("127.0.0.1:5000/a/b/c:x", "127.0.0.1:5000/a/b/c:x"),
            // A first component without `.` or `:` is a Docker Hub namespace.
            ("myhost/app", "docker.io/myhost/app:latest"),
            (
                &format!("nginx@{d}"),
                &format!("docker.io/library/nginx@{d}"),
            ),
            (
                &format!("nginx:1@{d}"),
                &format!("docker.io/library/nginx:1@{d}"),
            ),
        ] {
            let r: Reference = input.parse().unwrap();
            assert_eq!(r.to_string(), want, "{input}");
        }
    }

    #[test]
    fn targets_and_api_host() {
        let r: Reference = "nginx".parse().unwrap();
        assert_eq!(
            (r.api_host(), r.target().as_str()),
            ("registry-1.docker.io", "latest")
        );
        let d = Digest::of(b"m");
        let r: Reference = format!("ghcr.io/o/a:v1@{d}").parse().unwrap();
        assert_eq!((r.api_host(), r.target()), ("ghcr.io", d.to_string()));
    }

    #[test]
    fn rejects_unsafe_parts() {
        for bad in [
            "",
            "Nginx",
            "a/../b",
            "a//b",
            "ghcr.io/",
            "ghcr.io/a:",
            "ghcr.io/a:.x",
            "ghcr.io/a:b/c",
            "a?x=1",
            "a#frag",
            "a@sha256:abc",
            "host.io/a/.b",
            "bad_host.io/a",
            &format!("a:{}", "t".repeat(129)),
        ] {
            assert!(bad.parse::<Reference>().is_err(), "{bad}");
        }
    }
}
