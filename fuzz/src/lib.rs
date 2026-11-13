//! Shared helpers for the `dassie-image` flattener targets: run the whole
//! `scan`/`finish`/`emit` pipeline, and check the structural invariants of
//! the tar stream it writes (ADR 0007).

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use dassie_image::Tree;
use dassie_image::error::Flatten;
use tar::{Archive, EntryType};

/// What an output entry is, with the content that matters for comparing it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Dir,
    File(Vec<u8>),
    Symlink(Vec<u8>),
    /// The normalised target path.
    Hardlink(Vec<u8>),
    /// Any other entry type, by its type byte.
    Other(u8),
}

/// Flattens `layers` (bottom first) and appends `config`.
pub fn flatten(layers: &[&[u8]], config: &[u8]) -> Result<Vec<u8>, Flatten> {
    let mut tree = Tree::new();
    for layer in layers {
        tree.scan(*layer)?;
    }
    tree.finish()?;
    tree.emit(|i| Ok(layers[i]), config, Vec::new())
}

/// Parses the flattened stream and panics unless:
///
/// - the first entry is the root (`./`), a `0755 root:root` directory, and
///   no other entry is the root;
/// - no path is absolute or contains `..`, and none appears twice;
/// - every entry's parent directory appears earlier;
/// - every hard link's target appears earlier and is not a directory;
/// - `.dassie` and `.dassie/image.json` (holding `config`) are the last two
///   entries, and nothing else lives under `.dassie`.
///
/// Returns every other entry, root aside, by normalised path.
pub fn check_output(out: &[u8], config: &[u8]) -> BTreeMap<Vec<u8>, Kind> {
    let mut entries = Vec::new();
    let mut archive = Archive::new(out);
    for entry in archive.entries().expect("output is a tar stream") {
        let mut entry = entry.expect("output entry parses");
        let path = normalise(&entry.path_bytes());
        let header = entry.header();
        let ty = header.entry_type();
        if entries.is_empty() {
            assert!(
                path.is_empty()
                    && ty == EntryType::Directory
                    && header.mode().ok() == Some(0o755)
                    && header.uid().ok() == Some(0)
                    && header.gid().ok() == Some(0),
                "output does not start with a 0755 root:root root"
            );
        }
        let kind = match ty {
            EntryType::Directory => Kind::Dir,
            EntryType::Regular | EntryType::Continuous => {
                let mut data = Vec::new();
                entry.read_to_end(&mut data).expect("output entry data");
                Kind::File(data)
            }
            EntryType::Symlink => Kind::Symlink(link_name(&entry)),
            EntryType::Link => Kind::Hardlink(normalise(&link_name(&entry))),
            other => Kind::Other(other.as_byte()),
        };
        entries.push((path, kind));
    }

    entries.remove(0);
    let mut seen = HashSet::new();
    let mut dirs = HashSet::new();
    for (path, kind) in &entries {
        assert!(!path.is_empty(), "output has a second root entry: {entries:?}");
        assert!(seen.insert(path.clone()), "{} appears twice", show(path));
        if let Some(parent) = parent(path) {
            assert!(
                dirs.contains(parent),
                "{} comes before its parent directory",
                show(path)
            );
        }
        if let Kind::Hardlink(target) = kind {
            assert!(
                seen.contains(target) && !dirs.contains(target),
                "hard link {} -> {} does not follow a non-directory",
                show(path),
                show(target)
            );
        }
        if *kind == Kind::Dir {
            dirs.insert(path.clone());
        }
    }

    let n = entries.len();
    assert!(n >= 2, "output lacks /.dassie/image.json: {entries:?}");
    assert_eq!(entries[n - 2], (b".dassie".to_vec(), Kind::Dir));
    assert_eq!(
        entries[n - 1],
        (b".dassie/image.json".to_vec(), Kind::File(config.to_vec()))
    );
    entries.truncate(n - 2);
    for (path, _) in &entries {
        assert!(
            path != b".dassie" && !path.starts_with(b".dassie/"),
            "image content under .dassie survived: {}",
            show(path)
        );
    }
    entries.into_iter().collect()
}

/// `a/./b/` → `a/b`. Panics on an absolute path or a `..` component.
fn normalise(raw: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for c in Path::new(OsStr::from_bytes(raw)).components() {
        match c {
            Component::Normal(s) => parts.push(s.as_bytes()),
            Component::CurDir => {}
            other => panic!("{other:?} in output path {}", show(raw)),
        }
    }
    parts.join(&b'/')
}

fn link_name<R: Read>(entry: &tar::Entry<'_, R>) -> Vec<u8> {
    entry
        .link_name_bytes()
        .map(|l| l.into_owned())
        .unwrap_or_default()
}

fn parent(path: &[u8]) -> Option<&[u8]> {
    let i = path.iter().rposition(|&b| b == b'/')?;
    Some(&path[..i])
}

pub fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}
