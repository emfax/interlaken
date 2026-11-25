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
            ..FileOptions::default()
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

## Where content is kept

By default, chunks are stored in the engine table `engine_fs_chunks`, keyed by namespace and hash. Raft replicates them, and snapshots carry them. Values of 64 KiB or more are kept apart from keys on disk, so compactions don't rewrite file content. A database created before this keeps its old layout until a snapshot install rebuilds its data.

Every file's content then passes through Raft, which costs replication traffic and disk writes in proportion to the content stored. A namespace can keep its content in an S3-compatible object store instead, such as AWS S3, MinIO, Ceph or Cloudflare R2. Then only metadata, and a reference to each chunk's object, goes through Raft and into snapshots.

### Configuring object stores

Object stores need the `object-store` cargo feature, which embedded builds don't need. Each node is given the stores it can reach, by name:

```rust,ignore
use interlaken_engine::files::ObjectStoreConfig;

let options = EngineOptions {
    object_stores: vec![
        ObjectStoreConfig::s3("assets", "acme-files").region("eu-west-1"),
        ObjectStoreConfig::s3("onprem", "files")
            .endpoint("https://minio.internal:9000"),
    ],
    ..EngineOptions::default()
};
```

- **The database records only names.** A namespace's store name and key prefix are replicated, and so is the cluster's default store name. A store's endpoint, bucket and region are configuration on each node. Every node that serves a namespace must have its store configured; a node without it fails file operations there with `42P16`, and everything else keeps working.
- **Credentials come only from the node's environment,** by the standard AWS chain:
  - the instance's or pod's role;
  - a web identity token file (`AWS_WEB_IDENTITY_TOKEN_FILE` with `AWS_ROLE_ARN`);
  - container credentials;
  - `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`.

  There is no way to give keys in the configuration, so they are never replicated, logged or put in a snapshot. Prefer roles and token files, whose credentials are short-lived. Environment variables hold a long-lived key that child processes inherit and debugging tools can read, which suits development and simple on-prem setups.
- **TLS.** Endpoints must use TLS. `allow_http()` permits plain `http://` for local testing only.
- **Endpoints.** `endpoint(...)` points at a service other than AWS, and addresses buckets by path, as MinIO and most self-hosted services need.
- **Directory stores.** `ObjectStoreConfig::directory(name, path)` keeps objects in a directory, such as a network file system mounted on every node, and serves for testing.

### Choosing a namespace's store

```rust,ignore
use interlaken_engine::files::{FileOptions, FileStore};

session.set_default_file_store(Some("assets")).await?;  // admin
session.enable_files("app", &FileOptions::default()).await?;  // uses assets
session
    .enable_files("audit", &FileOptions { store: FileStore::Database, ..FileOptions::default() })
    .await?;
```

- **Choosing a store.** `FileOptions::store` is `Default` (the cluster's default store, or the database if there is none), `Database`, or `Object(name)`. The choice is fixed when files are enabled: changing the default later doesn't move existing namespaces, and moving a namespace between stores isn't supported yet.
- **The default store.** `set_default_file_store` needs `admin`, and refuses a store this node isn't configured with.
- **Key prefixes.** Objects are keyed `PREFIX/HASH-SUFFIX`, where the prefix is the namespace's name unless `FileOptions::prefix` says otherwise. Each staging of a chunk writes an object of its own, so collecting a chunk's old object never races with staging the same content again. Equal content written while a chunk is stored reuses its object.

### Consistency and failures

- **Writes.** A chunk's object is uploaded before the chunk is recorded, and a file refers only to recorded chunks. So a committed file never refers to an object that isn't there. A failed upload fails the write with `58030` and leaves no metadata.
- **Reads.** Readers fetch each chunk's object and check it against its hash. A missing or altered object fails with `XX001`.
- **Collection.** Collection deletes objects only after the chunks that referred to them are gone. Their keys wait in the engine table `engine_fs_trash` until their deletion succeeds, so a crash or a failed deletion is retried by the next collection. Dropping a namespace puts its objects in the trash the same way.
- **Policies.** Interlaken's policies decide who may read a file through the engine. Anyone holding the bucket's credentials can read every object directly, so give each cluster its own credentials with access to its bucket and prefix only, and turn on the bucket's encryption at rest.
