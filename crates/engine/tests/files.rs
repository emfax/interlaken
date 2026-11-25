//! Files and directories in the database (ADR-0044 phase 1).

mod support;

use interlaken_engine::catalog::EntitySpec;
use interlaken_engine::files::{CHUNK_SIZE, FileKind, FileOptions, ROOT_ID};
use interlaken_engine::policy::{PolicyDocument, PolicyLanguage};
use interlaken_engine::{Engine, EngineError, Session};
use interlaken_ir::security::EntityUid;
use support::session::{admin, create, session, table};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// Return user `n`.
fn user(n: i64) -> EntityUid {
    EntityUid::long("User", n)
}

/// Return the SQLSTATE of a call that must fail.
fn sqlstate<T>(result: Result<T, EngineError>) -> &'static str {
    result.map(|_| ()).unwrap_err().sqlstate()
}

/// Return `n` bytes of a repeating, position-dependent pattern.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

/// A database with users 1 to 3 as `User` entities, namespace `app` with
/// files owned by `User`s, and `policies` as the active Cedar set.
async fn database(dir: &std::path::Path, policies: &str) -> Engine {
    let engine = create(dir).await;
    let mut root = session(&engine, admin());
    root.create_table(&table("users", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    root.query(
        "(plan (insert users (values (columns a) (row 1) (row 2) (row 3)) (columns id)))",
        Vec::new(),
    )
    .await
    .unwrap();
    root.declare_entity(&EntitySpec {
        name: "User".into(),
        table: "users".into(),
        key: "id".into(),
        attributes: Vec::new(),
    })
    .await
    .unwrap();
    root.create_namespace("app", &admin()).await.unwrap();
    root.enable_files(
        "app",
        &FileOptions {
            owner_type: Some("User".into()),
            ..FileOptions::default()
        },
    )
    .await
    .unwrap();
    let id = root
        .submit_policies(
            &PolicyDocument {
                language: PolicyLanguage::Cedar,
                body: policies.into(),
            },
            "",
        )
        .await
        .unwrap();
    root.verify_policies(id).await.unwrap();
    root.activate_policies(id, true).await.unwrap();
    engine
}

/// Policies letting the administrator do anything, and users read
/// directories and read and write files they own.
const OWNERS: &str = r#"permit (principal is Admin, action, resource);
permit (principal is User, action == Action::"read", resource is app::File)
  when { resource.kind == "dir" };
permit (principal is User, action, resource is app::File)
  when { resource.kind == "file" && resource has owner && resource.owner == principal };"#;

/// Return the administrator's session.
fn root(engine: &Engine) -> Session {
    session(engine, admin())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert the operations of ADR-0044 section 5 on a small tree.
async fn operations_work() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path(), OWNERS).await;
    let mut s = root(&engine);
    let mut fs = s.files("app").await.unwrap();

    let top = fs.stat("/").await.unwrap();
    assert_eq!((top.id.as_str(), top.kind), (ROOT_ID, FileKind::Dir));
    fs.create_dir("/uploads").await.unwrap();
    fs.create_dir_all("/archive/2026/11").await.unwrap();
    let written = fs
        .write("/uploads/hello.txt", b"hello, files")
        .await
        .unwrap();
    assert_eq!(written.size, 12);
    assert_eq!(
        written.hash.as_deref(),
        Some(blake3::hash(b"hello, files").to_hex().as_str())
    );
    assert_eq!(written.owner, admin().to_string());
    assert_eq!(
        fs.read("/uploads/hello.txt").await.unwrap(),
        b"hello, files"
    );

    // Replacing keeps the id and changes the content.
    let replaced = fs.write("/uploads/hello.txt", b"bye").await.unwrap();
    assert_eq!(replaced.id, written.id);
    assert_eq!(fs.read("/uploads/hello.txt").await.unwrap(), b"bye");
    // An empty file has no chunks.
    let empty = fs.write("/uploads/empty", b"").await.unwrap();
    assert!(empty.chunks.is_empty());
    assert!(fs.read("/uploads/empty").await.unwrap().is_empty());

    let names: Vec<String> = fs
        .read_dir("/uploads")
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert_eq!(names, ["empty", "hello.txt"]);

    let moved = fs
        .rename("/uploads/hello.txt", "/archive/2026/hello.txt")
        .await
        .unwrap();
    assert_eq!(moved.id, written.id);
    assert!(!fs.exists("/uploads/hello.txt").await.unwrap());
    assert_eq!(fs.read("/archive/2026/hello.txt").await.unwrap(), b"bye");
    // Directories move with what they hold.
    fs.rename("/archive/2026", "/uploads/old").await.unwrap();
    assert_eq!(fs.read("/uploads/old/hello.txt").await.unwrap(), b"bye");

    // Errors.
    assert_eq!(sqlstate(fs.stat("/nope").await), "58P01");
    assert_eq!(sqlstate(fs.create_dir("/uploads").await), "58P02");
    assert_eq!(sqlstate(fs.create_dir("/missing/dir").await), "58P01");
    assert_eq!(sqlstate(fs.remove("/uploads").await), "2BP01");
    assert_eq!(sqlstate(fs.read_dir("/uploads/empty").await), "42809");
    assert_eq!(sqlstate(fs.open("/uploads").await), "42809");
    assert_eq!(sqlstate(fs.create("/uploads/empty/x").await), "42809");
    assert_eq!(
        sqlstate(fs.rename("/uploads", "/uploads/old/x").await),
        "22023"
    );
    assert_eq!(sqlstate(fs.remove("/").await), "22023");
    assert_eq!(sqlstate(fs.stat("relative").await), "22023");
    assert_eq!(sqlstate(fs.stat("/a/../b").await), "22023");

    fs.remove("/uploads/old/hello.txt").await.unwrap();
    fs.remove("/uploads/old/11").await.unwrap();
    fs.remove("/uploads/old").await.unwrap();
    assert!(!fs.exists("/uploads/old").await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a file of several chunks streams in and out, and seeks.
async fn large_files_stream() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path(), OWNERS).await;
    let mut s = root(&engine);
    let mut fs = s.files("app").await.unwrap();
    let content = pattern(CHUNK_SIZE * 3 + 12_345);

    let mut writer = fs.create("/big.bin").await.unwrap();
    // Odd-sized writes cross chunk boundaries.
    for part in content.chunks(100_003) {
        writer.write_all(part).await.unwrap();
    }
    writer.flush().await.unwrap();
    let meta = writer.finish().await.unwrap();
    assert_eq!(meta.size, content.len() as u64);
    assert_eq!(meta.chunks.len(), 4);
    assert_eq!(
        meta.hash.as_deref(),
        Some(blake3::hash(&content).to_hex().as_str())
    );

    let mut reader = fs.open("/big.bin").await.unwrap();
    let mut back = Vec::new();
    reader.read_to_end(&mut back).await.unwrap();
    assert!(back == content, "content differs");

    let mut reader = fs.open("/big.bin").await.unwrap();
    let at = CHUNK_SIZE as u64 * 2 - 10;
    reader.seek(std::io::SeekFrom::Start(at)).await.unwrap();
    let mut window = vec![0; 30];
    reader.read_exact(&mut window).await.unwrap();
    assert_eq!(window, content[at as usize..at as usize + 30]);
    reader.seek(std::io::SeekFrom::End(-5)).await.unwrap();
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail).await.unwrap();
    assert_eq!(tail, content[content.len() - 5..]);

    // Equal content is stored once and hashes the same.
    let copy = fs.write("/copy.bin", &content).await.unwrap();
    assert_eq!(copy.chunks, meta.chunks);
    assert_eq!(copy.hash, meta.hash);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert policies govern files like rows: users see directories and
/// their own files, can't read or replace others', and queries over
/// `fs_nodes` are filtered the same way.
async fn policies_govern_files() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path(), OWNERS).await;
    {
        let mut s = root(&engine);
        s.files("app")
            .await
            .unwrap()
            .create_dir("/shared")
            .await
            .unwrap();
    }
    let mut one = session(&engine, user(1));
    let mut fs = one.files("app").await.unwrap();
    let mine = fs.write("/shared/one.txt", b"one's").await.unwrap();
    assert_eq!(mine.owner, user(1).to_string());
    // Users can't create directories: the policies permit only reading them.
    assert_eq!(sqlstate(fs.create_dir("/mine").await), "42501");

    let mut two = session(&engine, user(2));
    let mut fs = two.files("app").await.unwrap();
    fs.write("/shared/two.txt", b"two's").await.unwrap();
    assert_eq!(sqlstate(fs.stat("/shared/one.txt").await), "58P01");
    assert_eq!(sqlstate(fs.read("/shared/one.txt").await), "58P01");
    let names: Vec<String> = fs
        .read_dir("/shared")
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert_eq!(names, ["two.txt"]);
    // A hidden file's name is still taken.
    assert_eq!(
        sqlstate(fs.write("/shared/one.txt", b"mine now").await),
        "58P02"
    );
    // A directory with hidden entries isn't empty.
    let mut s = root(&engine);
    let rows = s
        .query("(plan (get app.fs_nodes))", Vec::new())
        .await
        .unwrap();
    assert_eq!(rows.row_count(), 4, "the root, /shared and two files");
    let rows = two
        .query("(plan (get app.fs_nodes))", Vec::new())
        .await
        .unwrap();
    assert_eq!(rows.row_count(), 3, "user 2 doesn't see user 1's file");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert files are transactional: a rolled-back write leaves nothing,
/// and a committed one appears with the rest of its transaction.
async fn files_are_transactional() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path(), OWNERS).await;
    let mut s = root(&engine);
    s.begin(Default::default()).await.unwrap();
    s.files("app")
        .await
        .unwrap()
        .write("/draft", &pattern(CHUNK_SIZE + 1))
        .await
        .unwrap();
    assert!(
        s.files("app")
            .await
            .unwrap()
            .exists("/draft")
            .await
            .unwrap()
    );
    s.rollback().unwrap();
    assert!(
        !s.files("app")
            .await
            .unwrap()
            .exists("/draft")
            .await
            .unwrap()
    );

    s.begin(Default::default()).await.unwrap();
    s.files("app")
        .await
        .unwrap()
        .write("/kept", b"x")
        .await
        .unwrap();
    let mut other = root(&engine);
    assert!(
        !other
            .files("app")
            .await
            .unwrap()
            .exists("/kept")
            .await
            .unwrap(),
        "not visible before commit"
    );
    s.commit().await.unwrap();
    assert!(
        other
            .files("app")
            .await
            .unwrap()
            .exists("/kept")
            .await
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert files need enabling, only once, by an owner or administrator,
/// and go with their namespace.
async fn files_belong_to_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path(), OWNERS).await;
    let mut s = root(&engine);
    assert_eq!(sqlstate(s.files("public").await), "42P16");
    assert_eq!(
        sqlstate(s.enable_files("app", &FileOptions::default()).await),
        "42P16"
    );
    assert_eq!(
        sqlstate(
            session(&engine, user(1))
                .enable_files("public", &FileOptions::default())
                .await
        ),
        "42501"
    );
    s.files("app")
        .await
        .unwrap()
        .write("/f", b"data")
        .await
        .unwrap();
    // While the active policies name app::File, app must keep that type, so
    // the namespace is recreated in the same transaction.
    s.begin(Default::default()).await.unwrap();
    s.drop_namespace("app", true).await.unwrap();
    s.create_namespace("app", &admin()).await.unwrap();
    s.enable_files("app", &FileOptions::default())
        .await
        .unwrap();
    s.commit().await.unwrap();
    assert!(!s.files("app").await.unwrap().exists("/f").await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert quotas bound bytes and files exactly, usage follows every
/// change, and a writer stops as soon as it would pass the byte quota.
async fn quotas_bound_usage() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path(), OWNERS).await;
    let mut s = root(&engine);
    s.set_file_quota("app", Some(1000), Some(3)).await.unwrap();
    assert_eq!(
        sqlstate(
            session(&engine, user(1))
                .set_file_quota("app", None, None)
                .await
        ),
        "42501"
    );
    let mut fs = s.files("app").await.unwrap();
    fs.create_dir("/d").await.unwrap();
    fs.write("/d/a", &[1; 600]).await.unwrap();
    let usage = fs.usage().await.unwrap();
    assert_eq!((usage.bytes, usage.files), (600, 2));

    assert_eq!(sqlstate(fs.write("/d/b", &[2; 500]).await), "53400");
    let mut writer = fs.create("/d/c").await.unwrap();
    assert_eq!(
        writer.write_all(&[3; 401]).await.unwrap_err().kind(),
        std::io::ErrorKind::Other
    );
    drop(writer);
    // Replacing a file counts only the difference.
    fs.write("/d/a", &[4; 1000]).await.unwrap();
    assert_eq!(fs.usage().await.unwrap().bytes, 1000);
    fs.write("/d/a", &[5; 10]).await.unwrap();
    fs.write("/d/b", &[6; 10]).await.unwrap();
    assert_eq!(sqlstate(fs.create_dir("/e").await), "53400");
    fs.remove("/d/b").await.unwrap();
    let usage = fs.usage().await.unwrap();
    assert_eq!((usage.bytes, usage.files), (10, 2));
    fs.create_dir("/e").await.unwrap();

    // A failed operation leaves usage as it was.
    assert_eq!(sqlstate(fs.create_dir("/e").await), "58P02");
    assert_eq!(fs.usage().await.unwrap().files, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert collection removes chunks no file refers to once the grace
/// period has passed, and never chunks a file refers to.
async fn collection_removes_unreferenced_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path(), OWNERS).await;
    let mut s = root(&engine);
    let mut fs = s.files("app").await.unwrap();
    let kept = pattern(CHUNK_SIZE + 10);
    fs.write("/kept", &kept).await.unwrap();
    fs.write("/gone", &pattern(100)).await.unwrap();
    fs.write("/replaced", b"old").await.unwrap();
    fs.write("/replaced", b"new").await.unwrap();
    fs.remove("/gone").await.unwrap();
    // A writer that never finishes leaves its staged chunk.
    let mut abandoned = fs.create("/abandoned").await.unwrap();
    abandoned.write_bytes(&vec![9; CHUNK_SIZE]).await.unwrap();
    drop(abandoned);

    let young = engine
        .collect_files(std::time::Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!(young.chunks, 0, "everything is within the grace period");
    let collected = engine
        .collect_files(std::time::Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(
        collected.chunks, 3,
        "/gone, the old /replaced and the abandoned chunk"
    );
    assert_eq!(collected.bytes, 100 + 3 + CHUNK_SIZE as u64);
    let mut fs = s.files("app").await.unwrap();
    assert!(fs.read("/kept").await.unwrap() == kept);
    assert_eq!(fs.read("/replaced").await.unwrap(), b"new");
    assert_eq!(
        engine
            .collect_files(std::time::Duration::ZERO)
            .await
            .unwrap()
            .chunks,
        0
    );
}
