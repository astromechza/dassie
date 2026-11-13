//! Builds the rootfs artefact for an image: flatten, then `mkfs.ext4 -d -`
//! (ADR 0007).

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use flate2::read::MultiGzDecoder;

use crate::digest::{Digest, VerifyingReader};
use crate::flatten::{self, Tree};
use crate::oci::{Compression, Descriptor};
use crate::store::{self, Image, Store};

/// Bumped whenever the builder would produce a different filesystem from the
/// same image, so old artefacts are never reused or overwritten.
pub const FORMAT_VERSION: u32 = 1;

/// mkfs's own clock, as seconds since the epoch, so the same image always
/// builds to the same bytes.
///
/// e2fsprogs stamps the superblock (`s_mkfs_time`, `s_wtime`), the root
/// directory, `lost+found` and each inode's creation time (`crtime`) with
/// "now". `E2FSPROGS_FAKE_TIME` replaces that clock (e2fsprogs
/// `lib/ext2fs/initialize.c`). File times still come from the layers. The
/// value is `1` because e2fsprogs treats `0` as unset and uses the real clock
/// (`fs->now ? fs->now : time(0)`).
///
/// `SOURCE_DATE_EPOCH` would also fix the clock, but e2fsprogs then clamps
/// every inode time down to it (`clamped_time` in `misc/create_inode.c`),
/// which would erase the image's mtimes. So it is removed from mkfs's
/// environment.
const FAKE_TIME: &str = "1";

/// The filesystem label.
const LABEL: &str = "dassie";

/// Numbers each build's temporary file, so concurrent builds of one image in
/// one process never share it. The pid separates processes.
static BUILD_SEQ: AtomicU64 = AtomicU64::new(0);

/// Room for ext4 metadata and the journal on top of the content estimate.
const HEADROOM_MIB: u64 = 64;
/// Inodes on top of one per entry (`-N`).
///
/// mke2fs reserves inodes 1-10 and gives inode 11 to `lost+found`. The rest
/// is headroom for apps that create files while the root drive is still
/// booted read-write (until M1 slice C makes it read-only). 1024 inodes cost
/// 256 KiB of inode table at the default 256-byte inode size. mke2fs rounds
/// `-N` up to fill whole inode groups anyway, so the exact figure matters
/// little.
const SPARE_INODES: u64 = 1024;

/// What [`build`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BuildOutput {
    /// The read-only ext4 artefact (see [`artefact_path`]).
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// The `mkfs.ext4` to run. It needs e2fsprogs 1.47.1 or later, built with
    /// libarchive, for `-d -` (a tarball on stdin).
    pub mkfs: PathBuf,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mkfs: PathBuf::from("mkfs.ext4"),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Store(store::Error),
    Flatten(flatten::Error),
    UnsupportedLayer(Digest, String),
    Io(PathBuf, io::Error),
    Mkfs(ExitStatus, String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Flatten(e) => write!(f, "flatten: {e}"),
            Self::UnsupportedLayer(d, media_type) => {
                write!(f, "layer {d}: unsupported media type {media_type:?}")
            }
            Self::Io(path, e) => write!(f, "{}: {e}", path.display()),
            Self::Mkfs(status, stderr) => write!(
                f,
                "mkfs.ext4 failed ({status}): {}\n(it needs e2fsprogs >= 1.47.1 and the \
                 libarchive shared library, e.g. Debian's libarchive13t64)",
                stderr.trim()
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<store::Error> for Error {
    fn from(e: store::Error) -> Self {
        Self::Store(e)
    }
}

impl From<flatten::Error> for Error {
    fn from(e: flatten::Error) -> Self {
        Self::Flatten(e)
    }
}

/// The artefact for an image: `<out_dir>/sha256-<hex>.v<FORMAT_VERSION>.ext4`.
pub fn artefact_path(out_dir: &Path, image: &Digest) -> PathBuf {
    out_dir.join(format!("sha256-{}.v{FORMAT_VERSION}.ext4", image.hex()))
}

/// Builds the rootfs for `image` into `out_dir`, or returns the existing one.
///
/// The file is written to a temporary name and renamed into place, so a
/// crash never leaves a partial artefact at the final path. Artefacts are
/// read-only (`0444`) and never modified once written.
pub fn build(
    store: &Store,
    image: &Image,
    out_dir: &Path,
    opts: &Options,
) -> Result<BuildOutput, Error> {
    let path = artefact_path(out_dir, &image.digest);
    if path.exists() {
        return Ok(BuildOutput { path });
    }
    for layer in &image.manifest.layers {
        if Compression::of_layer(&layer.media_type).is_none() {
            return Err(Error::UnsupportedLayer(
                layer.digest.clone(),
                layer.media_type.clone(),
            ));
        }
    }

    let mut tree = Tree::new();
    for layer in &image.manifest.layers {
        tree.scan(open_layer(store, layer).map_err(flatten::Error::Io)?)?;
    }
    let stats = tree.finish()?;

    fs::create_dir_all(out_dir).map_err(|e| Error::Io(out_dir.to_path_buf(), e))?;
    let mut tmp_name = OsString::from(".");
    tmp_name.push(path.file_name().unwrap_or_default());
    let seq = BUILD_SEQ.fetch_add(1, Ordering::Relaxed);
    tmp_name.push(format!(".tmp.{}.{seq}", std::process::id()));
    let tmp = out_dir.join(tmp_name);

    let result = mkfs(store, image, &tree, stats, &tmp, opts).and_then(|()| {
        let io = |e| Error::Io(tmp.clone(), e);
        let file = File::open(&tmp).map_err(io)?;
        file.set_permissions(fs::Permissions::from_mode(0o444))
            .map_err(io)?;
        file.sync_all().map_err(io)?;
        fs::rename(&tmp, &path).map_err(io)?;
        File::open(out_dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| Error::Io(out_dir.to_path_buf(), e))
    });
    if result.is_err() {
        drop(fs::remove_file(&tmp));
    }
    result.map(|()| BuildOutput { path })
}

/// Runs `mkfs.ext4 -d -` on `tmp`, streaming the flattened tar to it.
fn mkfs(
    store: &Store,
    image: &Image,
    tree: &Tree,
    stats: flatten::Stats,
    tmp: &Path,
    opts: &Options,
) -> Result<(), Error> {
    let size_mib = (stats.bytes + stats.bytes / 4).div_ceil(1 << 20) + HEADROOM_MIB;
    let uuid = image.digest.uuid();
    let mut child = Command::new(&opts.mkfs)
        .env("E2FSPROGS_FAKE_TIME", FAKE_TIME)
        // Overrides the fake time and clamps inode times; see FAKE_TIME.
        .env_remove("SOURCE_DATE_EPOCH")
        .args([
            "-q",
            "-F",
            "-b",
            &flatten::BLOCK_SIZE.to_string(),
            "-L",
            LABEL,
        ])
        .args(["-U", &uuid, "-E"])
        .arg(format!("root_owner=0:0,hash_seed={uuid}"))
        .arg("-N")
        .arg((stats.entries + SPARE_INODES).to_string())
        .args(["-d", "-"])
        .arg(tmp)
        .arg(format!("{size_mib}M"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Io(opts.mkfs.clone(), e))?;

    // Drained on a thread: a chatty mkfs must not block while we write stdin.
    let stderr = child.stderr.take().map(|mut s| {
        thread::spawn(move || {
            let mut out = String::new();
            drop(s.read_to_string(&mut out));
            out
        })
    });
    let streamed = match child.stdin.take() {
        Some(stdin) => tree
            .emit(
                |i| layer_at(store, image, i),
                &image.config,
                BufWriter::new(stdin),
            )
            .and_then(|mut w| w.flush().map_err(flatten::Error::Io)),
        None => Ok(()),
    };
    let exit = child.wait().map_err(|e| Error::Io(opts.mkfs.clone(), e))?;
    let stderr = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
    // A failed mkfs closes its stdin, so its own error explains a write error.
    if !exit.success() {
        return Err(Error::Mkfs(exit, stderr));
    }
    streamed.map_err(Error::Flatten)
}

fn layer_at(store: &Store, image: &Image, i: usize) -> Result<LayerReader, flatten::Error> {
    let layer = image.manifest.layers.get(i).ok_or_else(|| {
        flatten::Error::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no layer {i}"),
        ))
    })?;
    open_layer(store, layer).map_err(flatten::Error::Io)
}

fn open_layer(store: &Store, layer: &Descriptor) -> io::Result<LayerReader> {
    let blob = store
        .open_blob(layer)
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e.to_string()))?;
    Ok(match Compression::of_layer(&layer.media_type) {
        Some(Compression::Gzip) => LayerReader::Gzip(MultiGzDecoder::new(blob)),
        _ => LayerReader::Plain(blob),
    })
}

/// A layer's tar stream. At the end of the decompressed data it drains the
/// compressed blob too, so its digest is always checked.
#[derive(Debug)]
enum LayerReader {
    Plain(VerifyingReader<File>),
    /// Every member: a gzip layer may be split across several.
    Gzip(MultiGzDecoder<VerifyingReader<File>>),
}

impl Read for LayerReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(r) => r.read(buf),
            Self::Gzip(r) => {
                let n = r.read(buf)?;
                if n == 0 && !buf.is_empty() {
                    io::copy(r.get_mut(), &mut io::sink())?;
                }
                Ok(n)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use flate2::Compression as Level;
    use flate2::write::GzEncoder;

    use super::*;
    use crate::Platform;
    use crate::flatten::tests::Layer;
    use crate::store::tests::{layout, put};

    #[test]
    fn gzip_layers_are_decoded_and_verified() {
        let tar = Layer::default().file("a", "hello", 0o644, 0).tar();
        let mut gz = GzEncoder::new(Vec::new(), Level::default());
        gz.write_all(&tar).unwrap();
        let gz = gz.finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        layout(dir.path(), "app", b"{}", &[]);
        let d = put(dir.path(), &gz);
        let store = Store::open(dir.path()).unwrap();
        let desc = Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            digest: d.clone(),
            size: gz.len() as u64,
            platform: None,
            annotations: BTreeMap::new(),
        };
        let mut out = Vec::new();
        open_layer(&store, &desc)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, tar);

        // A layer split across two gzip members decodes to the whole tar.
        let (head, tail) = tar.split_at(512);
        let mut two = Vec::new();
        for part in [head, tail] {
            let mut gz = GzEncoder::new(Vec::new(), Level::default());
            gz.write_all(part).unwrap();
            two.extend(gz.finish().unwrap());
        }
        let two_d = put(dir.path(), &two);
        let two_desc = Descriptor {
            digest: two_d,
            size: two.len() as u64,
            ..desc.clone()
        };
        let mut out = Vec::new();
        open_layer(&store, &two_desc)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, tar);

        // Bytes after the last gzip member still count towards the digest.
        let mut tampered = gz.clone();
        tampered.push(0);
        fs::write(store.blob_path(&d), &tampered).unwrap();
        let err = open_layer(&store, &desc)
            .unwrap()
            .read_to_end(&mut Vec::new())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_zstd_and_reuses_existing_artefacts() {
        let dir = tempfile::tempdir().unwrap();
        layout(dir.path(), "app", b"{}", &[]);
        let store = Store::open(dir.path()).unwrap();
        let mut image = store.image("app", &Platform::host()).unwrap();
        let out = tempfile::tempdir().unwrap();

        // An existing artefact is returned without running mkfs at all.
        let existing = artefact_path(out.path(), &image.digest);
        fs::write(&existing, b"").unwrap();
        let opts = Options {
            mkfs: PathBuf::from("/nonexistent/mkfs"),
        };
        assert_eq!(
            build(&store, &image, out.path(), &opts).unwrap().path,
            existing
        );

        image.manifest.layers.push(Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+zstd".into(),
            digest: Digest::of(b"z"),
            size: 1,
            platform: None,
            annotations: BTreeMap::new(),
        });
        image.digest = Digest::of(b"other");
        assert!(matches!(
            build(&store, &image, out.path(), &opts),
            Err(Error::UnsupportedLayer(..))
        ));
    }

    #[test]
    fn failed_mkfs_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let layer = Layer::default().file("a", "x", 0o644, 0).tar();
        layout(dir.path(), "app", b"{}", &[layer]);
        let store = Store::open(dir.path()).unwrap();
        let image = store.image("app", &Platform::host()).unwrap();
        let out = tempfile::tempdir().unwrap();
        let opts = Options {
            mkfs: PathBuf::from("false"),
        };
        assert!(matches!(
            build(&store, &image, out.path(), &opts),
            Err(Error::Mkfs(..))
        ));
        assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0);
    }
}
