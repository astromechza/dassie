//! Generates 1–4 small layers over a tiny path alphabet, so replacements,
//! nesting and whiteouts collide often, and flattens them. Checks the output's
//! structural invariants, that the same input always gives the same bytes,
//! and that the result matches a naive reference model of the OCI layer
//! rules (image-spec v1.1.0 `layer.md`, ADR 0007).
//!
//! The model is written from the spec, not from `flatten.rs`: it applies each
//! layer to an in-memory tree, whiteouts first (they only ever see lower
//! layers), then the entries in tar order.

#![no_main]

use std::collections::BTreeMap;

use arbitrary::{Arbitrary, Result, Unstructured};
use dassie_fuzz::{Kind, check_output, flatten, show};
use dassie_image::error::Flatten;
use libfuzzer_sys::fuzz_target;
use tar::{Builder, EntryType, Header};

const MAX_LAYERS: usize = 4;
const MAX_OPS: usize = 12;

fuzz_target!(|input: Input| {
    let mut layers = input.layers;
    layers.truncate(MAX_LAYERS);
    if layers.is_empty() {
        return;
    }
    for ops in &mut layers {
        ops.truncate(MAX_OPS);
    }
    let config = &input.config;

    let tars: Vec<Vec<u8>> = layers.iter().map(|ops| build_layer(ops)).collect();
    let tars: Vec<&[u8]> = tars.iter().map(Vec::as_slice).collect();
    let got = flatten(&tars, config);
    let again = flatten(&tars, config);
    let want = model(&layers);

    match got {
        Err(e) => {
            assert!(
                matches!(
                    e,
                    Flatten::DanglingLink { .. } | Flatten::LinkTargetChanged { .. }
                ),
                "unexpected error: {e}"
            );
            assert!(
                want.is_none(),
                "flattener rejected ({e}) what the model accepts: {:?}",
                want.as_ref().map(dump)
            );
            assert!(again.is_err(), "second run succeeded");
        }
        Ok(out) => {
            let entries = check_output(&out, config);
            let Some(want) = want else {
                panic!(
                    "flattener accepted a broken hard link: {:?}",
                    dump(&entries)
                );
            };
            assert!(
                entries == want,
                "flattener and model disagree\n  flattener: {:?}\n  model:     {:?}",
                dump(&entries),
                dump(&want)
            );
            assert!(again.ok() == Some(out), "second run differs");
        }
    }
});

#[derive(Arbitrary, Debug)]
struct Input {
    layers: Vec<Vec<Op>>,
    config: Vec<u8>,
}

#[derive(Arbitrary, Debug)]
enum Op {
    /// `.` or `./`.
    Root(Style),
    Dir(Path, Style),
    File(Path, Style, u8),
    Symlink(Path, Style, u8),
    /// Path, then target.
    Hardlink(Path, Style, Path, Style),
    /// `<parent>/.wh.<name>`.
    Whiteout(Parent, Name, Style),
    /// `<parent>/.wh..wh..opq`.
    Opaque(Parent, Style),
}

/// How a path is spelled in the tar header.
#[derive(Arbitrary, Debug, Clone, Copy)]
struct Style {
    /// `./` prefix.
    dot: bool,
    /// Trailing `/` on directories.
    slash: bool,
}

#[derive(Debug, Clone, Copy)]
struct Name(&'static str);

impl<'a> Arbitrary<'a> for Name {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        Ok(Self(match u.int_in_range(0..=6u8)? {
            0 | 1 => "a",
            2 | 3 => "b",
            4 | 5 => "c",
            _ => ".dassie",
        }))
    }
}

/// 1 to 3 names.
#[derive(Debug, Clone)]
struct Path(Vec<Name>);

impl<'a> Arbitrary<'a> for Path {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let n = u.int_in_range(1..=3)?;
        (0..n)
            .map(|_| Name::arbitrary(u))
            .collect::<Result<_>>()
            .map(Self)
    }
}

/// The root or 1 to 2 names, so a marker inside is at most 3 deep.
#[derive(Debug, Clone)]
struct Parent(Vec<Name>);

impl<'a> Arbitrary<'a> for Parent {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let n = u.int_in_range(0..=2)?;
        (0..n)
            .map(|_| Name::arbitrary(u))
            .collect::<Result<_>>()
            .map(Self)
    }
}

fn key(names: &[Name]) -> String {
    names.iter().map(|n| n.0).collect::<Vec<_>>().join("/")
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn content(v: u8) -> Vec<u8> {
    if v % 4 == 0 {
        Vec::new()
    } else {
        v.to_string().into_bytes()
    }
}

fn symlink_target(v: u8) -> &'static str {
    ["a", "../b", "/c", "b/c/a"][usize::from(v % 4)]
}

/// One op in canonical form: the path the model keys on, and what goes there.
enum Canon {
    Root,
    Dir(String),
    Put(String, Kind),
    Link(String, String),
    /// Marker path, then the path it removes.
    Whiteout(String, String),
    /// Marker path, then the directory it empties.
    Opaque(String, String),
}

impl Op {
    fn canon(&self) -> Canon {
        match self {
            Op::Root(_) => Canon::Root,
            Op::Dir(p, _) => Canon::Dir(key(&p.0)),
            Op::File(p, _, v) => Canon::Put(key(&p.0), Kind::File(content(*v))),
            Op::Symlink(p, _, v) => Canon::Put(
                key(&p.0),
                Kind::Symlink(symlink_target(*v).as_bytes().to_vec()),
            ),
            Op::Hardlink(p, _, t, _) => Canon::Link(key(&p.0), key(&t.0)),
            Op::Whiteout(parent, name, _) => {
                let parent = key(&parent.0);
                Canon::Whiteout(
                    join(&parent, &format!(".wh.{}", name.0)),
                    join(&parent, name.0),
                )
            }
            Op::Opaque(parent, _) => {
                let parent = key(&parent.0);
                Canon::Opaque(join(&parent, ".wh..wh..opq"), parent)
            }
        }
    }
}

// ---- Building the layer tar ----

fn spell(key: &str, style: Style, dir: bool) -> String {
    let mut s = if style.dot {
        format!("./{key}")
    } else {
        key.to_owned()
    };
    if dir && style.slash {
        s.push('/');
    }
    s
}

fn build_layer(ops: &[Op]) -> Vec<u8> {
    let mut b = Builder::new(Vec::new());
    for op in ops {
        match (op, op.canon()) {
            (Op::Root(s), _) => append(
                &mut b,
                if s.dot { "./" } else { "." },
                EntryType::Directory,
                &[],
                "",
            ),
            (Op::Dir(_, s), Canon::Dir(k)) => {
                append(&mut b, &spell(&k, *s, true), EntryType::Directory, &[], "");
            }
            (Op::File(_, s, v), Canon::Put(k, _)) => {
                append(
                    &mut b,
                    &spell(&k, *s, false),
                    EntryType::Regular,
                    &content(*v),
                    "",
                );
            }
            (Op::Symlink(_, s, v), Canon::Put(k, _)) => append(
                &mut b,
                &spell(&k, *s, false),
                EntryType::Symlink,
                &[],
                symlink_target(*v),
            ),
            (Op::Hardlink(_, s, _, ts), Canon::Link(k, t)) => append(
                &mut b,
                &spell(&k, *s, false),
                EntryType::Link,
                &[],
                &spell(&t, *ts, false),
            ),
            (
                Op::Whiteout(_, _, s) | Op::Opaque(_, s),
                Canon::Whiteout(m, _) | Canon::Opaque(m, _),
            ) => {
                append(&mut b, &spell(&m, *s, false), EntryType::Regular, &[], "");
            }
            _ => unreachable!(),
        }
    }
    b.into_inner().expect("in-memory tar")
}

/// Writes the header by hand, so `./` prefixes and trailing slashes reach the
/// flattener as spelled (`Builder` would normalise them away).
fn append(b: &mut Builder<Vec<u8>>, path: &str, ty: EntryType, data: &[u8], link: &str) {
    let mut h = Header::new_gnu();
    let old = h.as_old_mut();
    old.name[..path.len()].copy_from_slice(path.as_bytes());
    old.linkname[..link.len()].copy_from_slice(link.as_bytes());
    h.set_entry_type(ty);
    h.set_size(data.len() as u64);
    h.set_mode(if ty == EntryType::Directory {
        0o755
    } else {
        0o644
    });
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_cksum();
    b.append(&h, data).expect("in-memory tar");
}

// ---- The reference model ----

struct Node {
    /// Unique per inserted entry, so a hard link can tell its target was
    /// replaced by something else at the same path.
    id: u64,
    kind: Kind,
    /// For a hard link, the id of the node it linked to.
    link: Option<u64>,
}

type Fs = BTreeMap<String, Node>;

fn under(key: &str, dir: &str) -> bool {
    dir.is_empty() || key.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
}

fn is_dassie(key: &str) -> bool {
    key == ".dassie" || under(key, ".dassie")
}

fn remove_tree(fs: &mut Fs, key: &str) {
    fs.retain(|k, _| k != key && !under(k, key));
}

fn remove_children(fs: &mut Fs, dir: &str) {
    fs.retain(|k, _| !under(k, dir));
}

/// Makes every ancestor of `key` a directory, replacing non-directories.
fn imply_parents(fs: &mut Fs, key: &str, next: &mut u64) {
    for (i, _) in key.match_indices('/') {
        let a = &key[..i];
        if !fs.get(a).is_some_and(|n| n.kind == Kind::Dir) {
            insert(fs, a, Kind::Dir, None, next);
        }
    }
}

fn insert(fs: &mut Fs, key: &str, kind: Kind, link: Option<u64>, next: &mut u64) {
    remove_tree(fs, key);
    *next += 1;
    fs.insert(
        key.to_owned(),
        Node {
            id: *next,
            kind,
            link,
        },
    );
}

/// The flattened tree, or `None` if a hard link dangles or its target is
/// later replaced or removed.
fn model(layers: &[Vec<Op>]) -> Option<BTreeMap<Vec<u8>, Kind>> {
    let mut fs = Fs::new();
    let mut next = 0;
    for ops in layers {
        let ops: Vec<Canon> = ops.iter().map(Op::canon).collect();

        // Whiteouts and opaque markers act on the lower layers only, wherever
        // they sit in this layer's tar.
        for op in &ops {
            match op {
                Canon::Whiteout(m, gone) if !is_dassie(m) => remove_tree(&mut fs, gone),
                Canon::Opaque(m, dir) if !is_dassie(m) => remove_children(&mut fs, dir),
                _ => {}
            }
        }

        // Then this layer's entries in tar order; a later one at the same
        // path wins.
        for op in &ops {
            let path = match op {
                Canon::Root => continue,
                Canon::Dir(k) | Canon::Put(k, _) | Canon::Link(k, _) => k,
                Canon::Whiteout(m, _) | Canon::Opaque(m, _) => m,
            };
            if is_dassie(path) {
                continue;
            }
            // A hard link names whatever is at its target when the link is
            // reached, before its own parents are implied. (A link whose
            // parent replaces its target is broken, but only if it survives:
            // the final check below catches that.)
            let target = match op {
                Canon::Link(_, t) => Some(fs.get(t).filter(|n| n.kind != Kind::Dir)?.id),
                _ => None,
            };
            imply_parents(&mut fs, path, &mut next);
            match op {
                Canon::Dir(k) => {
                    if !fs.get(k).is_some_and(|n| n.kind == Kind::Dir) {
                        insert(&mut fs, k, Kind::Dir, None, &mut next);
                    }
                }
                Canon::Put(k, kind) => insert(&mut fs, k, kind.clone(), None, &mut next),
                Canon::Link(k, t) => {
                    let kind = Kind::Hardlink(t.as_bytes().to_vec());
                    insert(&mut fs, k, kind, target, &mut next);
                }
                Canon::Root | Canon::Whiteout(..) | Canon::Opaque(..) => {}
            }
        }
    }

    for node in fs.values() {
        if let (Some(id), Kind::Hardlink(t)) = (node.link, &node.kind) {
            let t = std::str::from_utf8(t).expect("model keys are UTF-8");
            if fs.get(t).map(|n| n.id) != Some(id) {
                return None;
            }
        }
    }
    Some(
        fs.into_iter()
            .map(|(k, n)| (k.into_bytes(), n.kind))
            .collect(),
    )
}

fn dump(entries: &BTreeMap<Vec<u8>, Kind>) -> Vec<String> {
    entries
        .iter()
        .map(|(k, v)| match v {
            Kind::Dir => format!("{}/", show(k)),
            Kind::File(d) => format!("{}={}", show(k), show(d)),
            Kind::Symlink(t) => format!("{}->{}", show(k), show(t)),
            Kind::Hardlink(t) => format!("{}=>{}", show(k), show(t)),
            Kind::Other(b) => format!("{}:{}", show(k), *b as char),
        })
        .collect()
}
