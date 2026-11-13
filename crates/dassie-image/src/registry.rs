//! A small synchronous registry client: pulls one image into the blob store
//! (ADR 0007).
//!
//! It speaks just enough of the distribution API for public images:
//! anonymous access, the bearer-token challenge Docker Hub and GHCR use, an
//! index down to the platform manifest, and blobs streamed into the store
//! with their digests checked. Credentials, private registries and retries
//! are out of scope for now.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use ureq::Agent;
use ureq::http::Response;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use crate::digest::Digest;
use crate::oci::{Compression, Descriptor, Manifest, OCI_MANIFEST, Platform};
use crate::reference::Reference;
use crate::store::{self, MAX_DEPTH, MAX_JSON, Node, Store, check_platform};

/// Every manifest shape the store can read. Docker schema 1 is not asked for.
const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.docker.distribution.manifest.v2+json";

/// The most layers an image may have. Docker has long refused more than
/// 127, so real images stay under it; the cap bounds the work a hostile
/// manifest can ask for.
pub const MAX_LAYERS: usize = 128;

#[derive(Debug)]
pub enum Error {
    /// The request failed before a status arrived, or its body broke off.
    Http(String, ureq::Error),
    /// An unexpected status, with the start of the body (registries explain
    /// errors in JSON there).
    Status(String, u16, String),
    /// The registry wants something other than an anonymous bearer token.
    Auth(String),
    Json(String, serde_json::Error),
    UnsupportedLayer(Digest, String),
    /// The manifest lists more than [`MAX_LAYERS`] layers.
    TooManyLayers(Digest, usize),
    Store(store::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(url, e) => write!(f, "GET {url}: {e}"),
            Self::Status(url, code, body) => write!(f, "GET {url}: HTTP {code} {body}"),
            Self::Auth(why) => write!(f, "registry auth: {why}"),
            Self::Json(what, e) => write!(f, "{what}: {e}"),
            Self::UnsupportedLayer(d, media_type) => {
                write!(f, "layer {d}: unsupported media type {media_type:?}")
            }
            Self::TooManyLayers(d, n) => {
                write!(
                    f,
                    "image {d} has {n} layers, over the limit of {MAX_LAYERS}"
                )
            }
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<store::Error> for Error {
    fn from(e: store::Error) -> Self {
        Self::Store(e)
    }
}

/// Progress of a pull, reported to the callback given to
/// [`Client::pull_with`] as it happens: [`Planned`](Self::Planned) once,
/// then events for the config and each layer blob in turn.
///
/// Overall progress is the sum of each blob's latest `done` (or its size,
/// once `Present` or `Stored`) over the sum of the planned sizes.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Event<'a> {
    /// The platform manifest is resolved and these blobs are next, before
    /// any is fetched. The pull can still fail after this (e.g. the config
    /// names another platform).
    Planned {
        digest: &'a Digest,
        config: &'a Descriptor,
        layers: &'a [Descriptor],
    },
    /// The blob is already in the store and will not be fetched.
    Present(&'a Descriptor),
    /// `done` of the blob's `size` bytes have arrived: first with `done` 0,
    /// then after every read.
    Fetching { blob: &'a Descriptor, done: u64 },
    /// The blob was verified and is in the store.
    Stored(&'a Descriptor),
}

/// What [`Client::pull`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Pulled {
    /// The platform manifest's digest: what desired state pins.
    pub digest: Digest,
    /// The `index.json` name now pointing at it: the normalized reference.
    pub name: String,
    /// Config and layer blobs downloaded, and how many bytes they held.
    pub blobs_fetched: usize,
    pub bytes_fetched: u64,
    /// Config and layer blobs that were already in the store.
    pub blobs_present: usize,
}

#[derive(Debug)]
pub struct Client {
    agent: Agent,
    scheme: &'static str,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// An HTTPS-only client that trusts the host's root certificates.
    pub fn new() -> Self {
        Self::with_scheme("https")
    }

    /// Plain HTTP, for the in-process fake registry in tests.
    #[cfg(test)]
    pub(crate) fn plain_http() -> Self {
        Self::with_scheme("http")
    }

    fn with_scheme(scheme: &'static str) -> Self {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::PlatformVerifier)
            .unversioned_rustls_crypto_provider(Arc::new(rustls_graviola::default_provider()))
            .build();
        let agent = Agent::config_builder()
            .tls_config(tls)
            .https_only(scheme == "https")
            // Statuses are handled here: a 401 carries the auth challenge.
            .http_status_as_error(false)
            // Redirects (blobs live on a CDN) are followed, and the bearer
            // token is dropped on the way (ureq's default), as storage
            // backends reject foreign Authorization headers.
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_per_read(Some(Duration::from_secs(60)))
            .user_agent(concat!("dassie-image/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { agent, scheme }
    }

    /// Pulls `reference` for `platform` into `store`, fetching only the
    /// blobs it lacks, and names the image there by the normalized
    /// reference.
    ///
    /// Blobs go in before the manifest, and the manifest before its name, so
    /// the store never names an image it cannot read. A reference pinned to
    /// a platform manifest that the store already holds in full is resolved
    /// without the network.
    pub fn pull(
        &self,
        store: &Store,
        reference: &Reference,
        platform: &Platform,
    ) -> Result<Pulled, Error> {
        self.pull_with(store, reference, platform, &mut |_| {})
    }

    /// [`pull`](Self::pull), reporting each blob's progress to `on`.
    pub fn pull_with(
        &self,
        store: &Store,
        reference: &Reference,
        platform: &Platform,
        on: &mut dyn FnMut(Event<'_>),
    ) -> Result<Pulled, Error> {
        let name = reference.to_string();
        if let Some(pulled) = local(store, reference, platform, &name, on)? {
            return Ok(pulled);
        }

        let mut session = Session {
            client: self,
            base: format!(
                "{}://{}/v2/{}",
                self.scheme,
                reference.api_host(),
                reference.repository()
            ),
            repository: reference.repository().to_owned(),
            token: None,
            on,
        };
        let mut target = reference.target();
        let mut want = reference.digest().cloned();
        for _ in 0..MAX_DEPTH {
            let (bytes, media_type) = session.manifest(&target)?;
            let digest = Digest::of(&bytes);
            if let Some(want) = want.as_ref().filter(|w| **w != digest) {
                return Err(store::Error::Tampered(want.clone()).into());
            }
            let node = serde_json::from_slice(&bytes)
                .map_err(|e| Error::Json(format!("manifest {digest}"), e))?;
            match node {
                Node::Index(index) => {
                    let pick = index
                        .select(platform)
                        .ok_or_else(|| store::Error::NoPlatform(digest, platform.clone()))?;
                    target = pick.digest.to_string();
                    want = Some(pick.digest.clone());
                }
                Node::Manifest(manifest) => {
                    return session.image(store, &bytes, &media_type, &manifest, platform, name);
                }
            }
        }
        Err(store::Error::TooDeep(want.unwrap_or_else(|| Digest::of(b""))).into())
    }
}

/// The pull of a reference pinned to a platform manifest whose blobs are all
/// in the store already: no network needed.
fn local(
    store: &Store,
    reference: &Reference,
    platform: &Platform,
    name: &str,
    on: &mut dyn FnMut(Event<'_>),
) -> Result<Option<Pulled>, Error> {
    let Some(digest) = reference.digest() else {
        return Ok(None);
    };
    let Ok(image) = store.image(&digest.to_string(), platform) else {
        return Ok(None);
    };
    if !image
        .manifest
        .layers
        .iter()
        .all(|l| store.has_blob(&l.digest))
    {
        return Ok(None);
    }
    check_layers(&image.digest, &image.manifest)?;
    on(Event::Planned {
        digest: &image.digest,
        config: &image.manifest.config,
        layers: &image.manifest.layers,
    });
    on(Event::Present(&image.manifest.config));
    for layer in &image.manifest.layers {
        on(Event::Present(layer));
    }
    // The resolved manifest, which differs from `digest` if a layout written
    // by `skopeo copy --all` holds the index too.
    let size = std::fs::metadata(store.blob_path(&image.digest)).map_or(0, |m| m.len());
    store.set_ref(
        name,
        &descriptor(media_type(&image.manifest, ""), &image.digest, size),
    )?;
    Ok(Some(Pulled {
        digest: image.digest,
        name: name.to_owned(),
        blobs_fetched: 0,
        bytes_fetched: 0,
        blobs_present: image.manifest.layers.len() + 1,
    }))
}

/// Refuses a manifest with too many layers, or layers the build cannot
/// unpack (zstd, foreign).
fn check_layers(digest: &Digest, manifest: &Manifest) -> Result<(), Error> {
    if manifest.layers.len() > MAX_LAYERS {
        return Err(Error::TooManyLayers(digest.clone(), manifest.layers.len()));
    }
    match manifest
        .layers
        .iter()
        .find(|l| Compression::of_layer(&l.media_type).is_none())
    {
        Some(l) => Err(Error::UnsupportedLayer(
            l.digest.clone(),
            l.media_type.clone(),
        )),
        None => Ok(()),
    }
}

/// A manifest's media type for its `index.json` descriptor, which must have
/// one: the body's field, else the response's `Content-Type`, else OCI's.
/// An OCI manifest may omit the field; a Docker v2 one never does.
fn media_type<'a>(manifest: &'a Manifest, content_type: &'a str) -> &'a str {
    [manifest.media_type.as_str(), content_type]
        .into_iter()
        .find(|t| !t.is_empty())
        .unwrap_or(OCI_MANIFEST)
}

/// An `index.json` entry for a manifest.
fn descriptor(media_type: &str, digest: &Digest, size: u64) -> Descriptor {
    Descriptor {
        media_type: media_type.to_owned(),
        digest: digest.clone(),
        size,
        platform: None,
        annotations: BTreeMap::new(),
    }
}

/// A token endpoint's answer. Docker Hub sends both fields; the spec allows
/// either.
#[derive(Deserialize)]
struct Token {
    token: Option<String>,
    access_token: Option<String>,
}

/// One pull from one repository, holding its bearer token.
struct Session<'a> {
    client: &'a Client,
    /// `<scheme>://<host>/v2/<repository>`
    base: String,
    repository: String,
    token: Option<String>,
    on: &'a mut dyn FnMut(Event<'_>),
}

impl Session<'_> {
    /// Fetches what `manifest` (whose bytes are `bytes`) needs, then stores
    /// the manifest and names it.
    fn image(
        &mut self,
        store: &Store,
        bytes: &[u8],
        content_type: &str,
        manifest: &Manifest,
        platform: &Platform,
        name: String,
    ) -> Result<Pulled, Error> {
        let mut pulled = Pulled {
            digest: Digest::of(bytes),
            name,
            blobs_fetched: 0,
            bytes_fetched: 0,
            blobs_present: 0,
        };
        // Refuse before downloading anything that could not be built.
        check_layers(&pulled.digest, manifest)?;
        if manifest.config.size > MAX_JSON {
            let path = store.blob_path(&manifest.config.digest);
            return Err(store::Error::TooLarge(path).into());
        }
        (self.on)(Event::Planned {
            digest: &pulled.digest,
            config: &manifest.config,
            layers: &manifest.layers,
        });
        self.blob(store, &manifest.config, &mut pulled)?;
        let config = store.read_small(&manifest.config.digest)?;
        check_platform(&pulled.digest, &config, platform)?;
        for layer in &manifest.layers {
            self.blob(store, layer, &mut pulled)?;
        }

        let size = bytes.len() as u64;
        store.put(&pulled.digest, size, bytes)?;
        let media_type = media_type(manifest, content_type);
        store.set_ref(&pulled.name, &descriptor(media_type, &pulled.digest, size))?;
        Ok(pulled)
    }

    /// A manifest's bytes (at most [`MAX_JSON`]) and its media type.
    fn manifest(&mut self, target: &str) -> Result<(Vec<u8>, String), Error> {
        let url = format!("{}/manifests/{target}", self.base);
        let resp = self.get(&url, Some(ACCEPT))?;
        let media_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or_default().trim().to_owned())
            .unwrap_or_default();
        let bytes = read_json(resp, &url)?;
        Ok((bytes, media_type))
    }

    /// Streams a blob into the store, unless it is there already.
    fn blob(&mut self, store: &Store, desc: &Descriptor, pulled: &mut Pulled) -> Result<(), Error> {
        if store.has_blob(&desc.digest) {
            pulled.blobs_present += 1;
            (self.on)(Event::Present(desc));
            return Ok(());
        }
        let url = format!("{}/blobs/{}", self.base, desc.digest);
        let resp = self.get(&url, None)?;
        (self.on)(Event::Fetching {
            blob: desc,
            done: 0,
        });
        let body = Counting {
            inner: resp.into_body().into_reader(),
            blob: desc,
            done: 0,
            on: &mut *self.on,
        };
        store.put(&desc.digest, desc.size, body)?;
        (self.on)(Event::Stored(desc));
        pulled.blobs_fetched += 1;
        pulled.bytes_fetched = pulled.bytes_fetched.saturating_add(desc.size);
        Ok(())
    }

    /// GETs `url`. On a 401 with a bearer challenge, fetches an anonymous
    /// token and tries once more; that also renews a token that expired
    /// partway through a pull.
    fn get(&mut self, url: &str, accept: Option<&str>) -> Result<Response<ureq::Body>, Error> {
        let mut renewed = false;
        loop {
            let mut req = self.client.agent.get(url);
            if let Some(accept) = accept {
                req = req.header("Accept", accept);
            }
            if let Some(token) = &self.token {
                req = req.header("Authorization", format!("Bearer {token}"));
            }
            let resp = req.call().map_err(|e| Error::Http(url.to_owned(), e))?;
            match resp.status().as_u16() {
                200 => return Ok(resp),
                401 if !renewed => {
                    let challenge = resp
                        .headers()
                        .get("www-authenticate")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    self.token = Some(self.fetch_token(&challenge)?);
                    renewed = true;
                }
                code => return Err(status_error(url, code, resp)),
            }
        }
    }

    /// Answers `WWW-Authenticate: Bearer realm=…,service=…,scope=…` with an
    /// anonymous token request to the realm.
    fn fetch_token(&self, challenge: &str) -> Result<String, Error> {
        let params = parse_bearer(challenge)
            .ok_or_else(|| Error::Auth(format!("unsupported challenge {challenge:?}")))?;
        let realm = params
            .get("realm")
            .ok_or_else(|| Error::Auth(format!("no realm in {challenge:?}")))?;
        if !realm.starts_with(&format!("{}://", self.client.scheme)) {
            return Err(Error::Auth(format!(
                "realm {realm:?} is not {}",
                self.client.scheme
            )));
        }
        let scope = params
            .get("scope")
            .cloned()
            .unwrap_or_else(|| format!("repository:{}:pull", self.repository));
        let mut req = self.client.agent.get(realm).query("scope", &scope);
        if let Some(service) = params.get("service") {
            req = req.query("service", service);
        }
        let resp = req.call().map_err(|e| Error::Http(realm.clone(), e))?;
        if resp.status() != 200 {
            return Err(status_error(realm, resp.status().as_u16(), resp));
        }
        let body = read_json(resp, realm)?;
        let t: Token = serde_json::from_slice(&body)
            .map_err(|e| Error::Json(format!("token from {realm}"), e))?;
        t.token
            .or(t.access_token)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| Error::Auth(format!("{realm} returned no token")))
    }
}

/// Reports [`Event::Fetching`] after every read of a blob body.
struct Counting<'a, 'b, R> {
    inner: R,
    blob: &'a Descriptor,
    done: u64,
    on: &'b mut dyn FnMut(Event<'_>),
}

impl<R: Read> Read for Counting<'_, '_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.done += n as u64;
            (self.on)(Event::Fetching {
                blob: self.blob,
                done: self.done,
            });
        }
        Ok(n)
    }
}

fn read_json(resp: Response<ureq::Body>, url: &str) -> Result<Vec<u8>, Error> {
    resp.into_body()
        .with_config()
        .limit(MAX_JSON)
        .read_to_vec()
        .map_err(|e| Error::Http(url.to_owned(), e))
}

fn status_error(url: &str, code: u16, resp: Response<ureq::Body>) -> Error {
    let body = resp
        .into_body()
        .with_config()
        .limit(4096)
        .lossy_utf8(true)
        .read_to_string()
        .unwrap_or_default();
    let body: String = body.trim().chars().take(300).collect();
    Error::Status(url.to_owned(), code, body)
}

/// The parameters of a `Bearer` challenge, keys lowercased. Values may be
/// quoted, and a quoted value may hold commas (`scope="repository:a:pull,push"`).
fn parse_bearer(header: &str) -> Option<BTreeMap<String, String>> {
    let (scheme, mut rest) = header.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let mut params = BTreeMap::new();
    loop {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        if rest.is_empty() {
            return Some(params);
        }
        let (key, after) = rest.split_once('=')?;
        // RFC 9110 allows optional whitespace around `=`.
        let after = after.trim_start_matches([' ', '\t']);
        let (value, after) = if let Some(q) = after.strip_prefix('"') {
            unquote(q)?
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after.get(..end)?.trim().to_owned(), after.get(end..)?)
        };
        params.insert(key.trim().to_ascii_lowercase(), value);
        rest = after;
    }
}

/// Splits a quoted-string (after its opening `"`) into its unescaped value
/// and what follows the closing `"`.
fn unquote(s: &str) -> Option<(String, &str)> {
    let mut value = String::new();
    let mut chars = s.char_indices();
    loop {
        match chars.next()? {
            (_, '\\') => value.push(chars.next()?.1),
            (i, '"') => return Some((value, s.get(i + 1..)?)),
            (_, c) => value.push(c),
        }
    }
}

#[cfg(test)]
mod fake;

#[cfg(test)]
mod tests;
