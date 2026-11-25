//! File content in object stores (ADR-0044 phase 3).
//!
//! A directory store stands in for S3 everywhere; `s3_compatible_store`
//! runs against a real S3-compatible service when
//! `INTERLAKEN_TEST_S3_ENDPOINT` and `INTERLAKEN_TEST_S3_BUCKET` name one,
//! with credentials from the usual `AWS_*` environment.

#![cfg(feature = "object-store")]

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use interlaken_engine::files::{CHUNK_SIZE, FileOptions, FileStore, ObjectStoreConfig};
use interlaken_engine::policy::{PolicyDocument, PolicyLanguage};
use interlaken_engine::{Engine, EngineError, EngineOptions, Session};
use interlaken_ir::security::SecurityContext;
use support::session::{admin, options};

/// Return the SQLSTATE of a call that must fail.
fn sqlstate<T>(result: Result<T, EngineError>) -> &'static str {
    result.map(|_| ()).unwrap_err().sqlstate()
}

/// Return `n` bytes of a repeating, position-dependent pattern.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 13 + i / 509) as u8).collect()
}

/// Return every file below `dir`.
fn objects(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(objects(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// Create a database in `dir` whose node knows `stores`, letting the
/// administrator do anything.
async fn database(dir: &Path, stores: Vec<ObjectStoreConfig>) -> Engine {
    let engine = Engine::create(
        dir,
        EngineOptions {
            object_stores: stores,
            ..options()
        },
        &admin(),
    )
    .await
    .unwrap();
    let mut root = session(&engine);
    let id = root
        .submit_policies(
            &PolicyDocument {
                language: PolicyLanguage::Cedar,
                body: "permit (principal is Admin, action, resource);".into(),
            },
            "",
        )
        .await
        .unwrap();
    root.verify_policies(id).await.unwrap();
    root.activate_policies(id, true).await.unwrap();
    engine
}

/// Return the administrator's session.
fn session(engine: &Engine) -> Session {
    engine.session(SecurityContext::new(admin()))
}

/// Enable files in a new namespace `name` with `store`.
async fn enable(root: &mut Session, name: &str, store: FileStore) {
    root.create_namespace(name, &admin()).await.unwrap();
    root.enable_files(
        name,
        &FileOptions {
            store,
            ..FileOptions::default()
        },
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a namespace's content goes to its object store, not the
/// database, and reads back; equal content is uploaded once.
async fn content_lives_in_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = tempfile::tempdir().unwrap();
    let engine = database(
        dir.path(),
        vec![ObjectStoreConfig::directory("local", bucket.path())],
    )
    .await;
    let mut root = session(&engine);
    enable(&mut root, "app", FileStore::Object("local".into())).await;
    let content = pattern(CHUNK_SIZE * 2 + 99);
    let mut fs = root.files("app").await.unwrap();
    assert_eq!(fs.object_store(), Some("local"));
    let meta = fs.write("/big", &content).await.unwrap();
    assert_eq!(meta.chunks.len(), 3);
    let stored = objects(bucket.path());
    assert_eq!(stored.len(), 3, "one object per chunk: {stored:?}");
    assert!(
        stored
            .iter()
            .all(|p| p.starts_with(bucket.path().join("app"))),
        "keys start with the namespace's prefix"
    );
    assert!(fs.read("/big").await.unwrap() == content);
    fs.write("/copy", &content).await.unwrap();
    assert_eq!(
        objects(bucket.path()).len(),
        3,
        "equal chunks are stored once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert the cluster's default store applies to namespaces enabled after
/// it is set, a namespace can choose the database instead, and a store
/// the node lacks is refused.
async fn the_default_store_applies_to_new_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = tempfile::tempdir().unwrap();
    let engine = database(
        dir.path(),
        vec![ObjectStoreConfig::directory("local", bucket.path())],
    )
    .await;
    let mut root = session(&engine);
    enable(&mut root, "before", FileStore::Default).await;
    assert_eq!(
        sqlstate(root.set_default_file_store(Some("elsewhere")).await),
        "42P16"
    );
    root.set_default_file_store(Some("local")).await.unwrap();
    assert_eq!(
        root.default_file_store().await.unwrap().as_deref(),
        Some("local")
    );
    enable(&mut root, "after", FileStore::Default).await;
    enable(&mut root, "inside", FileStore::Database).await;

    for (namespace, store) in [("before", None), ("after", Some("local")), ("inside", None)] {
        let mut fs = root.files(namespace).await.unwrap();
        assert_eq!(fs.object_store(), store, "{namespace}");
        fs.write("/f", namespace.as_bytes()).await.unwrap();
        assert_eq!(fs.read("/f").await.unwrap(), namespace.as_bytes());
    }
    assert_eq!(
        objects(bucket.path()).len(),
        1,
        "only `after` uses the store"
    );

    root.create_namespace("missing", &admin()).await.unwrap();
    let refused = root
        .enable_files(
            "missing",
            &FileOptions {
                store: FileStore::Object("nowhere".into()),
                ..FileOptions::default()
            },
        )
        .await;
    assert_eq!(sqlstate(refused), "42P16");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a corrupt or missing object is detected on read.
async fn corrupt_objects_are_detected() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = tempfile::tempdir().unwrap();
    let engine = database(
        dir.path(),
        vec![ObjectStoreConfig::directory("local", bucket.path())],
    )
    .await;
    let mut root = session(&engine);
    enable(&mut root, "app", FileStore::Object("local".into())).await;
    let mut fs = root.files("app").await.unwrap();
    fs.write("/a", b"original").await.unwrap();
    fs.write("/b", b"another").await.unwrap();
    let mut stored = objects(bucket.path());
    stored.sort();
    for path in &stored {
        let bytes = std::fs::read(path).unwrap();
        if bytes == b"original" {
            std::fs::write(path, b"tampered").unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
    }
    assert_eq!(sqlstate(fs.read("/a").await), "XX001");
    assert_eq!(sqlstate(fs.read("/b").await), "XX001");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert collection deletes the objects of collected chunks and of
/// dropped namespaces, and keeps every object a file refers to.
async fn collection_deletes_objects() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = tempfile::tempdir().unwrap();
    let engine = database(
        dir.path(),
        vec![ObjectStoreConfig::directory("local", bucket.path())],
    )
    .await;
    let mut root = session(&engine);
    enable(&mut root, "app", FileStore::Object("local".into())).await;
    enable(&mut root, "gone", FileStore::Object("local".into())).await;
    let mut fs = root.files("app").await.unwrap();
    fs.write("/kept", b"kept").await.unwrap();
    fs.write("/replaced", b"old").await.unwrap();
    fs.write("/replaced", b"new").await.unwrap();
    root.files("gone")
        .await
        .unwrap()
        .write("/x", b"x")
        .await
        .unwrap();
    assert_eq!(objects(bucket.path()).len(), 4);

    root.drop_namespace("gone", true).await.unwrap();
    let collected = engine.collect_files(Duration::ZERO).await.unwrap();
    assert_eq!(collected.chunks, 1, "the old /replaced");
    assert_eq!(
        collected.objects, 2,
        "it and the dropped namespace's object"
    );
    assert_eq!(objects(bucket.path()).len(), 2);
    let mut fs = root.files("app").await.unwrap();
    assert_eq!(fs.read("/kept").await.unwrap(), b"kept");
    assert_eq!(fs.read("/replaced").await.unwrap(), b"new");
    assert_eq!(
        engine.collect_files(Duration::ZERO).await.unwrap(),
        Default::default()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert files work against a real S3-compatible service, when the
/// environment names one; otherwise the test does nothing.
async fn s3_compatible_store() {
    let (Ok(endpoint), Ok(bucket)) = (
        std::env::var("INTERLAKEN_TEST_S3_ENDPOINT"),
        std::env::var("INTERLAKEN_TEST_S3_BUCKET"),
    ) else {
        eprintln!("skipped: INTERLAKEN_TEST_S3_ENDPOINT and INTERLAKEN_TEST_S3_BUCKET are unset");
        return;
    };
    let mut store = ObjectStoreConfig::s3("s3", bucket).endpoint(&endpoint);
    if endpoint.starts_with("http://") {
        store = store.allow_http();
    }
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path(), vec![store]).await;
    let mut root = session(&engine);
    let namespace = format!(
        "t{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros()
    );
    enable(&mut root, &namespace, FileStore::Object("s3".into())).await;
    let content = pattern(CHUNK_SIZE + 7);
    let mut fs = root.files(&namespace).await.unwrap();
    fs.write("/f", &content).await.unwrap();
    assert!(fs.read("/f").await.unwrap() == content);
    fs.remove("/f").await.unwrap();
    let collected = engine.collect_files(Duration::ZERO).await.unwrap();
    assert_eq!(collected.objects, 2);
}
