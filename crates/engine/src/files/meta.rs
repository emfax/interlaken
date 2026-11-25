//! What a file or directory is, as its metadata row says (ADR-0044).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use interlaken_ir::interp::Datum;

use crate::EngineError;

/// The id of every namespace's root directory, so policies can name it as
/// `File::"root"`.
pub const ROOT_ID: &str = "root";

/// The columns a lookup projects, in the order [`FileMeta::from_row`] reads.
pub(crate) const META_COLUMNS: &str =
    "n.id n.parent n.name n.kind n.owner n.size n.hash n.chunks n.created n.modified";

/// Whether a node is a file or a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// A directory.
    Dir,
    /// A file.
    File,
}

impl FileKind {
    /// Return the kind as stored: `dir` or `file`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dir => "dir",
            Self::File => "file",
        }
    }
}

/// A file's or directory's metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMeta {
    /// Its id: `root` for the root, generated otherwise. It stays the same
    /// when the node is renamed, and identifies it to policies.
    pub id: String,
    /// The id of its directory; `None` for the root.
    pub parent: Option<String>,
    /// Its name within its directory; empty for the root.
    pub name: String,
    /// A file or a directory.
    pub kind: FileKind,
    /// The principal that created it, as `Type::"key"`.
    pub owner: String,
    /// Its size in bytes; 0 for a directory.
    pub size: u64,
    /// The BLAKE3 hash of a file's content, as hex; `None` for a directory.
    pub hash: Option<String>,
    /// The hashes of a file's chunks, in order.
    pub chunks: Vec<String>,
    /// When it was created.
    pub created: SystemTime,
    /// When its content or name last changed.
    pub modified: SystemTime,
}

/// Return the error for a metadata row of the wrong shape.
fn malformed(what: &str) -> EngineError {
    EngineError::Catalog(format!("malformed file metadata: {what}"))
}

/// Return the text of `datum`.
fn text(datum: &Datum, what: &str) -> Result<String, EngineError> {
    match datum {
        Datum::Text(t) => Ok(t.clone()),
        _ => Err(malformed(what)),
    }
}

/// Return the time of a `timestamptz` datum.
fn time(datum: &Datum) -> Result<SystemTime, EngineError> {
    match datum {
        Datum::Timestamptz(micros) => {
            let offset = Duration::from_micros(micros.unsigned_abs());
            Ok(if *micros >= 0 {
                UNIX_EPOCH + offset
            } else {
                UNIX_EPOCH - offset
            })
        }
        _ => Err(malformed("time")),
    }
}

impl FileMeta {
    /// Read the metadata of one row projected as [`META_COLUMNS`].
    pub(crate) fn from_row(row: &[Datum]) -> Result<Self, EngineError> {
        let [
            id,
            parent,
            name,
            kind,
            owner,
            size,
            hash,
            chunks,
            created,
            modified,
        ] = row
        else {
            return Err(malformed("column count"));
        };
        Ok(Self {
            id: text(id, "id")?,
            parent: match parent {
                Datum::Null => None,
                other => Some(text(other, "parent")?),
            },
            name: text(name, "name")?,
            kind: match text(kind, "kind")?.as_str() {
                "dir" => FileKind::Dir,
                "file" => FileKind::File,
                _ => return Err(malformed("kind")),
            },
            owner: text(owner, "owner")?,
            size: match size {
                Datum::Int8(n) => u64::try_from(*n).map_err(|_| malformed("size"))?,
                _ => return Err(malformed("size")),
            },
            hash: match hash {
                Datum::Null => None,
                other => Some(text(other, "hash")?),
            },
            chunks: match chunks {
                Datum::Null => Vec::new(),
                Datum::Array(items) => items
                    .iter()
                    .map(|c| text(c, "chunk"))
                    .collect::<Result<_, _>>()?,
                _ => return Err(malformed("chunks")),
            },
            created: time(created)?,
            modified: time(modified)?,
        })
    }

    /// Return `true` for a directory.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.kind == FileKind::Dir
    }
}
