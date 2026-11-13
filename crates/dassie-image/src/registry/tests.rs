use std::fs;
use std::path::Path;

use serde_json::json;

use super::fake::Fake;
use super::*;

const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const TAR: &str = "application/vnd.oci.image.layer.v1.tar";

/// Registers an image's config and layer blobs with the fake and returns its
/// (unregistered) Docker v2 manifest.
fn image(fake: &Fake, arch: &str, layers: &[(&str, &[u8])]) -> Vec<u8> {
    let config = format!(r#"{{"os":"linux","architecture":"{arch}"}}"#);
    let config_d = fake.blob(config.as_bytes());
    let layers: Vec<_> = layers
        .iter()
        .map(|(media_type, bytes)| {
            json!({ "mediaType": media_type, "digest": fake.blob(bytes).to_string(), "size": bytes.len() })
        })
        .collect();
    serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": DOCKER_MANIFEST,
        "config": {
            "mediaType": "application/vnd.docker.container.image.v1+json",
            "digest": config_d.to_string(),
            "size": config.len(),
        },
        "layers": layers,
    }))
    .unwrap()
}

/// A two-platform image tagged `v1` whose platforms share a base layer.
/// Returns the amd64 and arm64 manifest digests.
fn multi_arch(fake: &Fake) -> (Digest, Digest) {
    let amd = image(fake, "amd64", &[(TAR, b"base"), (TAR, b"amd-top")]);
    let arm = image(fake, "arm64", &[(TAR, b"base"), (TAR, b"arm-top")]);
    let (amd_d, arm_d) = (Digest::of(&amd), Digest::of(&arm));
    fake.manifest(&amd_d.to_string(), DOCKER_MANIFEST, &amd);
    fake.manifest(&arm_d.to_string(), DOCKER_MANIFEST, &arm);
    let entry = |d: &Digest, len: usize, arch: &str| {
        json!({ "mediaType": DOCKER_MANIFEST, "digest": d.to_string(), "size": len,
                "platform": { "os": "linux", "architecture": arch } })
    };
    let list = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": DOCKER_LIST,
        "manifests": [entry(&amd_d, amd.len(), "amd64"), entry(&arm_d, arm.len(), "arm64")],
    }))
    .unwrap();
    fake.manifest("v1", DOCKER_LIST, &list);
    (amd_d, arm_d)
}

fn reference(fake: &Fake, rest: &str) -> Reference {
    format!("{}/team/app{rest}", fake.addr).parse().unwrap()
}

/// An event as `<kind> <first 8 hex> [done]`, for comparing.
fn event(e: Event<'_>) -> String {
    let short = |d: &Descriptor| d.digest.hex()[..8].to_owned();
    match e {
        Event::Present(d) => format!("present {}", short(d)),
        Event::Fetching { blob, done } => format!("fetching {} {done}", short(blob)),
        Event::Stored(d) => format!("stored {}", short(d)),
    }
}

/// Temporary files anywhere in the store.
fn leftovers(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for dir in [root.to_path_buf(), root.join("blobs/sha256")] {
        for e in fs::read_dir(dir).unwrap() {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            if name.contains(".tmp.") {
                found.push(name);
            }
        }
    }
    found
}

#[test]
fn pulls_through_token_challenge_and_index_then_dedups() {
    let fake = Fake::start();
    let (amd_d, arm_d) = multi_arch(&fake);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let client = Client::plain_http();
    let arm64 = Platform::new("linux", "arm64");
    let tag = reference(&fake, ":v1");

    let mut events = Vec::new();
    let pulled = client
        .pull_with(&store, &tag, &arm64, &mut |e| events.push(event(e)))
        .unwrap();
    assert_eq!(pulled.digest, arm_d);
    assert_eq!(pulled.name, format!("{}/team/app:v1", fake.addr));
    assert_eq!((pulled.blobs_fetched, pulled.blobs_present), (3, 0));
    let config = r#"{"os":"linux","architecture":"arm64"}"#;
    assert_eq!(
        pulled.bytes_fetched,
        (config.len() + "base".len() + "arm-top".len()) as u64
    );
    assert_eq!(fake.count("/token"), 1, "one token serves the whole pull");
    assert_eq!(fake.count("/storage/"), 3);
    // The pulled image reads back by name, verified, for the right platform.
    assert_eq!(store.image(&pulled.name, &arm64).unwrap().digest, arm_d);
    assert!(!store.has_blob(&amd_d));

    // A re-pull by tag fetches the manifests again, but no blobs.
    // Each blob: fetching from 0, up to its size, then stored.
    let size = |s: &str| s.len() as u64;
    for (blob, len) in [(config, size(config)), ("base", 4), ("arm-top", 7)] {
        let hex = Digest::of(blob.as_bytes()).hex()[..8].to_owned();
        let mine: Vec<_> = events
            .iter()
            .filter(|e| e.contains(&hex))
            .cloned()
            .collect();
        assert_eq!(
            mine.first(),
            Some(&format!("fetching {hex} 0")),
            "{events:?}"
        );
        assert!(
            mine.contains(&format!("fetching {hex} {len}")),
            "{events:?}"
        );
        assert_eq!(mine.last(), Some(&format!("stored {hex}")), "{events:?}");
    }

    let mut events = Vec::new();
    let again = client
        .pull_with(&store, &tag, &arm64, &mut |e| events.push(event(e)))
        .unwrap();
    assert_eq!(
        events.iter().filter(|e| e.starts_with("present")).count(),
        3
    );
    assert_eq!(events.len(), 3);
    assert_eq!(again.digest, arm_d);
    assert_eq!((again.blobs_fetched, again.blobs_present), (0, 3));
    assert_eq!(fake.count("/storage/"), 3);

    // Pinned to a digest the store holds in full: no request at all.
    let before = fake.count("/");
    let pinned = client
        .pull(&store, &reference(&fake, &format!("@{arm_d}")), &arm64)
        .unwrap();
    assert_eq!((pinned.digest, pinned.blobs_fetched), (arm_d, 0));
    assert_eq!(fake.count("/"), before);

    // The other platform shares the base layer.
    let amd = client
        .pull(&store, &tag, &Platform::new("linux", "amd64"))
        .unwrap();
    assert_eq!(amd.digest, amd_d);
    assert_eq!((amd.blobs_fetched, amd.blobs_present), (2, 1));
    assert_eq!(leftovers(dir.path()), Vec::<String>::new());
}

#[test]
fn tampered_blob_is_rejected_and_not_stored() {
    let fake = Fake::start();
    let manifest = image(&fake, "arm64", &[(TAR, b"layer-B")]);
    fake.manifest("v1", DOCKER_MANIFEST, &manifest);
    let layer = Digest::of(b"layer-B");
    fake.state
        .lock()
        .unwrap()
        .blobs
        .insert(layer.hex().to_owned(), b"layer-X".to_vec());

    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let err = Client::plain_http()
        .pull(
            &store,
            &reference(&fake, ":v1"),
            &Platform::new("linux", "arm64"),
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::Store(store::Error::Tampered(ref d)) if *d == layer),
        "{err}"
    );
    assert!(!store.has_blob(&layer));
    assert!(!store.has_blob(&Digest::of(&manifest)));
    assert_eq!(leftovers(dir.path()), Vec::<String>::new());
    let name = format!("{}/team/app:v1", fake.addr);
    assert!(matches!(
        store.image(&name, &Platform::new("linux", "arm64")),
        Err(store::Error::NotFound(_))
    ));
}

#[test]
fn interrupted_blob_leaves_no_partial_file() {
    let fake = Fake::start();
    let big = vec![7u8; 200_000];
    let manifest = image(&fake, "arm64", &[(TAR, &big)]);
    fake.manifest("v1", DOCKER_MANIFEST, &manifest);
    let layer = Digest::of(&big);
    fake.state
        .lock()
        .unwrap()
        .truncate
        .insert(layer.hex().to_owned());

    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let err = Client::plain_http()
        .pull(
            &store,
            &reference(&fake, ":v1"),
            &Platform::new("linux", "arm64"),
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Store(store::Error::Read(..) | store::Error::WrongSize(..))
        ),
        "{err}"
    );
    assert!(!store.has_blob(&layer));
    assert_eq!(leftovers(dir.path()), Vec::<String>::new());
}

#[test]
fn pinned_manifest_must_match_its_digest() {
    let fake = Fake::start();
    let manifest = image(&fake, "arm64", &[]);
    let pinned = Digest::of(b"something else");
    fake.manifest(&pinned.to_string(), DOCKER_MANIFEST, &manifest);

    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let err = Client::plain_http()
        .pull(
            &store,
            &reference(&fake, &format!("@{pinned}")),
            &Platform::new("linux", "arm64"),
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::Store(store::Error::Tampered(ref d)) if *d == pinned),
        "{err}"
    );
}

#[test]
fn refuses_before_downloading() {
    let fake = Fake::start();
    let zstd = image(
        &fake,
        "arm64",
        &[("application/vnd.oci.image.layer.v1.tar+zstd", b"z")],
    );
    fake.manifest("zstd", DOCKER_MANIFEST, &zstd);
    let deep: Vec<(&str, &[u8])> = vec![(TAR, b"l"); MAX_LAYERS + 1];
    let deep = image(&fake, "arm64", &deep);
    fake.manifest("deep", DOCKER_MANIFEST, &deep);
    multi_arch(&fake);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let client = Client::plain_http();
    let arm64 = Platform::new("linux", "arm64");

    let err = client
        .pull(&store, &reference(&fake, ":zstd"), &arm64)
        .unwrap_err();
    assert!(matches!(err, Error::UnsupportedLayer(..)), "{err}");
    let err = client
        .pull(&store, &reference(&fake, ":deep"), &arm64)
        .unwrap_err();
    assert!(matches!(err, Error::TooManyLayers(_, 129)), "{err}");
    let err = client
        .pull(
            &store,
            &reference(&fake, ":v1"),
            &Platform::new("linux", "riscv64"),
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::Store(store::Error::NoPlatform(..))),
        "{err}"
    );
    let err = client
        .pull(&store, &reference(&fake, ":nope"), &arm64)
        .unwrap_err();
    assert!(
        matches!(err, Error::Status(_, 404, ref body) if body.contains("MANIFEST_UNKNOWN")),
        "{err}"
    );
    assert_eq!(fake.count("/storage/"), 0);
}

#[test]
fn local_resolution_checks_layers_and_types_the_descriptor() {
    let fake = Fake::start();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let put = |bytes: &[u8]| {
        let d = Digest::of(bytes);
        store.put(&d, bytes.len() as u64, bytes).unwrap();
        d
    };
    let config = br#"{"os":"linux","architecture":"arm64"}"#;
    // OCI manifests may omit `mediaType`; skopeo-imported ones can be zstd.
    let manifest = |layer_type: &str| {
        let m = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "config": { "mediaType": "c", "digest": put(config).to_string(), "size": config.len() },
            "layers": [{ "mediaType": layer_type, "digest": put(b"l").to_string(), "size": 1 }],
        }))
        .unwrap();
        put(&m)
    };
    let client = Client::plain_http();
    let arm64 = Platform::new("linux", "arm64");

    let zstd = manifest("application/vnd.oci.image.layer.v1.tar+zstd");
    let err = client
        .pull(&store, &reference(&fake, &format!("@{zstd}")), &arm64)
        .unwrap_err();
    assert!(matches!(err, Error::UnsupportedLayer(..)), "{err}");

    let tar = manifest(TAR);
    let pulled = client
        .pull(&store, &reference(&fake, &format!("@{tar}")), &arm64)
        .unwrap();
    assert_eq!((pulled.digest, pulled.blobs_fetched), (tar, 0));
    assert_eq!(fake.count("/"), 0);
    let index: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join("index.json")).unwrap()).unwrap();
    assert_eq!(index["manifests"][0]["mediaType"], OCI_MANIFEST);
    assert_eq!(index["manifests"].as_array().unwrap().len(), 1);
}

#[test]
fn parses_bearer_challenges() {
    let p = parse_bearer(
        r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:a/b:pull,push""#,
    )
    .unwrap();
    assert_eq!(p["realm"], "https://auth.docker.io/token");
    assert_eq!(p["service"], "registry.docker.io");
    assert_eq!(p["scope"], "repository:a/b:pull,push");
    let p = parse_bearer(r#"bearer Realm=https://x/t, scope="a\"b""#).unwrap();
    assert_eq!(
        (p["realm"].as_str(), p["scope"].as_str()),
        ("https://x/t", "a\"b")
    );
    let p = parse_bearer("Bearer realm = \"https://x/t\",\tservice =\t\"s\"").unwrap();
    assert_eq!(
        (p["realm"].as_str(), p["service"].as_str()),
        ("https://x/t", "s")
    );
    assert!(parse_bearer(r#"Basic realm="x""#).is_none());
    assert!(parse_bearer(r#"Bearer realm="unterminated"#).is_none());
}
