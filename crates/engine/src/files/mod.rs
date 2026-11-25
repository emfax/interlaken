//! Files and directories inside the database (ADR-0044).

mod chunks;
mod collect;
mod fs;
mod meta;
mod path;
mod reader;
mod settings;
mod writer;

pub use chunks::CHUNK_SIZE;
pub(crate) use chunks::drop_namespace_chunks;
pub use collect::Collected;
pub use fs::Files;
pub use meta::{FileKind, FileMeta, ROOT_ID};
pub use path::MAX_NAME_BYTES;
pub use reader::FileReader;
pub(crate) use settings::forget as forget_file_namespace;
pub use settings::{FileOptions, FileUsage};
pub use writer::FileWriter;
