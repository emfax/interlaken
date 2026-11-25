# Files

A namespace can hold files and directories as well as tables. They are stored in the database, so they are:
- replicated with everything else;
- written in transactions with rows;
- reported by the change feed;
- governed by the same policies as rows.

File content is stored in chunks of up to 1 MiB, written and read as streams, so a file can be much larger than a transaction's write set or a statement's memory budget.

## Enabling files

```rust
# use interlaken_engine::catalog::{ColumnSpec, EntitySpec, TableSpec};
# use interlaken_engine::files::FileOptions;
# use interlaken_engine::{Engine, EngineOptions};
# use interlaken_ir::security::{EntityUid, SecurityContext};
# use interlaken_ir::text::parse_type;
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::str("Admin", "root");
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin.clone()));
# session.create_table(&TableSpec {
#     name: "users".into(),
#     columns: vec![ColumnSpec::new("id", parse_type("int8")?)],
#     primary_key: vec!["id".into()],
# }).await?;
# session.declare_entity(&EntitySpec {
#     name: "User".into(), table: "users".into(), key: "id".into(), attributes: Vec::new(),
# }).await?;
session.create_namespace("app", &admin).await?;
session
    .enable_files(
        "app",
        &FileOptions {
            owner_type: Some("User".into()),
            quota_bytes: Some(10 << 30),
            quota_files: None,
        },
    )
    .await?;
# Ok(())
# }
```

`enable_files` needs `admin` or ownership of the namespace. It creates:
- **`app.fs_nodes`:** one row per file or directory, with the following columns:
  - `id`: `root` for the root directory, and generated for everything else. It doesn't change when the node is renamed.
  - `parent`, `name` and `kind` (`dir` or `file`).
  - `owner`: the creating principal, as text such as `User::"1"`.
  - `owner_id`: the creating principal's key, when the principal is of the owner type.
  - `size`, `hash` and `chunks`: the BLAKE3 hash of the content, and of each chunk.
  - `created` and `modified`.
- **`app.fs_tree`:** the closure of the directory tree.
- **The entity type `app.File`** over `fs_nodes`, with the attributes `name`, `kind`, `size` and, given an `owner_type`, `owner`. Every node is a member of its directory, so `resource in app::File::"<directory id>"` holds for everything below a directory.

Everything is in the namespace: its owner owns the files' schema, and dropping the namespace with cascade drops its files and their content.

## Working with files

```rust,ignore
let mut fs = session.files("app").await?;
fs.create_dir_all("/uploads/2026").await?;

// Write a stream: the writer is a tokio AsyncWrite.
let mut writer = fs.create("/uploads/2026/report.pdf").await?;
tokio::io::copy(&mut body, &mut writer).await?;
let meta = writer.finish().await?;

// Or write and read whole files.
fs.write("/notes.txt", b"hello").await?;
let bytes = fs.read("/notes.txt").await?;

// Read a stream: the reader is a tokio AsyncRead and AsyncSeek.
let mut reader = fs.open("/uploads/2026/report.pdf").await?;

for entry in fs.read_dir("/uploads/2026").await? {
    println!("{} {} bytes", entry.name, entry.size);
}
fs.rename("/uploads/2026/report.pdf", "/archive/report.pdf").await?;
fs.remove("/archive/report.pdf").await?;
```

- **Paths** start with `/`, and names are separated by `/`. A name can't be empty, `.` or `..`, can't contain NUL, and is at most 255 bytes.
- **No links or permission bits.** There are no hard or symbolic links, and policies take the place of permission bits.
- **Whole replacement.** `create` replaces an existing file whole: files are never changed in place.
- **Moving directories.** `rename` moves a directory with everything in it. Moving a directory into itself fails.
- **Removing directories.** `remove` removes a file or an empty directory. A directory holding entries the principal can't see is still not empty.

### Transactions

Every operation runs in the session's transaction, or in an implicit one of its own, like a statement. A file written in a transaction appears, changes or disappears when that transaction commits, together with the rows it wrote, and a rollback undoes it. Two transactions changing the same node conflict as row writes do (`40001`).

Content is the exception: the writer stores each full chunk in a short transaction of its own as it is written, so memory and transactions stay bounded however large the file. A writer dropped before `finish`, or one whose transaction rolls back, leaves unreferenced chunks behind, which [collection](#collection) removes.

A reader holds the file's metadata as it was when opened. It reads one chunk at a time and checks each against its hash, failing with `XX001` if a chunk is missing or corrupt.

## Policies over files

File operations are statements on `fs_nodes`, so the policies on `app.File` decide them:

| Operation | Action and resource |
| --- | --- |
| `stat`, `exists`, `open`, `read`, `read_dir`, and every directory a path passes through | `read` on the node |
| `create_dir`, and `create` or `write` of a new file | `insert` on the new node |
| `create` or `write` replacing a file, `rename` | `update` on the node |
| `remove` | `delete` on the node |

```cedar,ignore
permit (principal is User, action == Action::"read", resource is app::File)
  when { resource.kind == "dir" };
permit (principal is User, action, resource is app::File)
  when { resource.kind == "file" && resource has owner && resource.owner == principal };
```

Under these policies:
- users can read every directory, and read and write their own files;
- a file a user may not read reads as missing (`58P01`), and directory listings leave it out;
- a name another user's file holds is still taken (`58P02`).

`fs_nodes` can also be read with ordinary queries, which the same policies filter. Clients can't reach file content except through a file they may read.

## Quotas

`quota_bytes` limits the bytes of file content a namespace holds, counting each file's size. `quota_files` limits how many files and directories it holds, not counting the root.

- **Changing quotas.** `Session::set_file_quota(namespace, bytes, files)` changes them, which needs `admin` or ownership of the namespace.
- **Usage.** `Files::usage()` returns what the namespace uses. Usage is kept exact: each writing transaction updates it.
- **Errors.** A write that would pass a quota fails with `53400`. A writer stops as soon as the file would pass the byte quota, rather than when it finishes.
- **Concurrency.** Concurrent writers in one namespace conflict on its usage, and one of them retries.

## Collection

Content no file refers to stays until `Engine::collect_files(grace)` removes it. This covers files that were removed or replaced, writers that never finished, and rolled-back transactions. Collection deletes the chunks of every namespace that no file or directory refers to and that weren't written within `grace`, and returns how many chunks and bytes it removed.

Run it periodically with a `grace` longer than any write, together with the transaction it finishes in, can take. An hour is a reasonable default. Equal content is stored once per namespace, and writing it again refreshes the chunk, so reusing content is safe.

## Storage

Chunks are stored in the engine table `engine_fs_chunks`, keyed by namespace and hash. Raft replicates them, and snapshots carry them. Values of 64 KiB or more are kept apart from keys on disk, so compactions don't rewrite file content. A database created before this keeps its old layout until a snapshot install rebuilds its data.

Every file's content passes through Raft, which costs replication traffic and disk writes in proportion to the content stored. Keeping content in object storage instead is designed (ADR-0044 phase 3) but not built.
