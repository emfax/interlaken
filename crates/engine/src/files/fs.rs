//! A namespace's files: the operations of ADR-0044 section 5.
//!
//! Metadata changes are ordinary DML on `ns.fs_nodes`, run through the
//! session's statement path, so policies, the change feed, replication and
//! transactions apply to them as to any row.

use interlaken_ir::catalog::Catalog;
use interlaken_ir::ddl::{
    AttributeSpec, ColumnSpec, DdlStatement, EntitySpec, ForeignKeySpec, IndexKey, IndexSpec,
    MembershipSpec, TableSpec,
};
use interlaken_ir::interp::Datum;
use interlaken_ir::security::{EntityKey, EntityUid};
use interlaken_ir::text::{parse_type, print_type};
use interlaken_ir::{Ident, Type};
use std::time::{SystemTime, UNIX_EPOCH};

use interlaken_storage::TxnOptions;
use interlaken_storage::encoding::Datum as Stored;

use super::chunks;
use super::meta::{FileKind, FileMeta, META_COLUMNS, ROOT_ID};
use super::path::{split, split_last};
use super::reader::FileReader;
use super::settings::{self, FileOptions, FileUsage, Settings};
use super::writer::FileWriter;
use crate::catalog::{self, ObjectKind};
use crate::session::pipeline::Step;
use crate::session::{Kind, may_change};
use crate::{EngineError, Session};

/// A namespace's files, reached through a session: every operation runs
/// as the session's principal, in its transaction or an implicit one.
pub struct Files<'s> {
    /// The session.
    pub(crate) session: &'s mut Session,
    /// The namespace's file settings.
    pub(crate) settings: Settings,
}

/// Return the qualified name of `namespace`'s object `name`.
fn object(namespace: &str, name: &str) -> String {
    format!("{namespace}.{name}")
}

/// Return the type `text` names.
fn ty(text: &str) -> Type {
    parse_type(text).expect("a built-in type")
}

/// Return `principal` as stored in `owner`: `Type::"key"`.
fn owner_text(principal: &EntityUid) -> String {
    principal.to_string()
}

/// Return the DDL statements that make `settings.namespace`'s file
/// tables, before and after the root's row is written: the table and its
/// constraints first, then the entity type and membership that put it
/// under policy control.
fn file_schema(settings: &Settings) -> (Vec<DdlStatement>, Vec<DdlStatement>) {
    let ns = settings.namespace.as_str();
    let nodes = object(ns, "fs_nodes");
    let tree = object(ns, "fs_tree");
    let entity = object(ns, "File");
    let owner_id = settings.owner_key_type.as_deref().unwrap_or("text");
    let columns = [
        ("id", "text".to_owned()),
        ("parent", "(option text)".to_owned()),
        ("name", "text".to_owned()),
        ("kind", "text".to_owned()),
        ("owner", "text".to_owned()),
        ("owner_id", format!("(option {owner_id})")),
        ("size", "int8".to_owned()),
        ("hash", "(option text)".to_owned()),
        ("chunks", "(option (array text))".to_owned()),
        ("created", "timestamptz".to_owned()),
        ("modified", "timestamptz".to_owned()),
    ];
    let before = vec![
        DdlStatement::CreateTable(TableSpec {
            name: nodes.clone(),
            columns: columns
                .iter()
                .map(|(n, t)| ColumnSpec::new(*n, ty(t)))
                .collect(),
            primary_key: vec!["id".into()],
        }),
        DdlStatement::CreateIndex(IndexSpec {
            name: "fs_nodes_by_name".into(),
            table: nodes.clone(),
            columns: vec![IndexKey::asc("parent"), IndexKey::asc("name")],
            unique: true,
            nones_distinct: true,
        }),
        DdlStatement::CreateTable(TableSpec {
            name: tree.clone(),
            columns: vec![
                ColumnSpec::new("descendant", ty("text")),
                ColumnSpec::new("ancestor", ty("text")),
                ColumnSpec::new("paths", ty("int8")),
            ],
            primary_key: vec!["descendant".into(), "ancestor".into()],
        }),
        DdlStatement::CreateForeignKey(ForeignKeySpec::new(
            "fs_nodes_parent",
            nodes.clone(),
            ["parent"],
            nodes.clone(),
            ["id"],
        )),
    ];
    let mut attributes = vec![
        AttributeSpec::new("name"),
        AttributeSpec::new("kind"),
        AttributeSpec::new("size"),
    ];
    if let Some(owner_type) = &settings.owner_type {
        attributes.push(AttributeSpec::reference("owner", "owner_id", owner_type));
    }
    let after = vec![
        DdlStatement::DeclareEntity(EntitySpec {
            name: entity.clone(),
            table: nodes.clone(),
            key: "id".into(),
            attributes,
        }),
        DdlStatement::DeclareMembership(MembershipSpec {
            member: entity.clone(),
            group: entity,
            edges: nodes,
            edge_columns: ("id".into(), "parent".into()),
            closure: tree,
            closure_columns: ("descendant".into(), "ancestor".into(), "paths".into()),
        }),
    ];
    (before, after)
}

/// Return the IR type of the key of entity or principal type `stored`.
fn owner_key_type(step: &Step<'_>, stored: &str) -> Result<String, EngineError> {
    let ident = Ident::new(stored);
    if let Some(entity) = step.catalog.entity_by_name(&ident) {
        let table = step
            .catalog
            .table_by_id(entity.table)
            .ok_or_else(|| EngineError::Catalog(format!("entity type {stored} has no table")))?;
        let key = table
            .columns
            .iter()
            .find(|c| c.id == entity.key)
            .ok_or_else(|| EngineError::Catalog(format!("entity type {stored} has no key")))?;
        return Ok(print_type(&Type {
            optional: false,
            ..key.ty.clone()
        }));
    }
    if step.catalog.principal_types().contains(&ident) {
        return Ok("text".into());
    }
    Err(EngineError::InvalidDefinition(format!(
        "{stored} is neither an entity type nor a principal type"
    )))
}

impl Session {
    /// Give namespace `namespace` files (ADR-0044): its tables `fs_nodes`
    /// (one row per file or directory) and `fs_tree` (the directory
    /// closure), the entity type `File` over them, with each node a member
    /// of its directory, and a root directory with id `root`.
    ///
    /// It needs `admin` or ownership of the namespace, and runs as one
    /// statement sequence in the open transaction or an implicit one.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::PermissionDenied`], or
    /// [`EngineError::InvalidDefinition`] if the namespace doesn't exist,
    /// has files already, or has objects of the same names.
    pub async fn enable_files(
        &mut self,
        namespace: &str,
        options: &FileOptions,
    ) -> Result<(), EngineError> {
        let implicit = !self.in_transaction();
        if implicit {
            self.begin(TxnOptions::default()).await?;
        }
        let result = self.enable_files_in_open(namespace, options).await;
        if !implicit {
            return result;
        }
        match result {
            Ok(()) => self.commit().await.map(|_| ()),
            Err(e) => {
                if self.in_transaction() {
                    self.rollback()?;
                }
                Err(e)
            }
        }
    }

    /// Run [`Session::enable_files`] in the open transaction.
    async fn enable_files_in_open(
        &mut self,
        namespace: &str,
        options: &FileOptions,
    ) -> Result<(), EngineError> {
        let settings = self
            .run(Kind::Catalog, async |step: &Step<'_>| {
                may_change(step, &[namespace]).await?;
                if !catalog::namespace_exists(step.txn, namespace).await? {
                    return Err(EngineError::InvalidDefinition(format!(
                        "namespace {namespace} does not exist"
                    )));
                }
                chunks::chunks_table(step.txn).await?;
                let (owner_type, owner_key_type) = match &options.owner_type {
                    None => (None, None),
                    Some(name) => {
                        let stored = catalog::resolve_existing(
                            step.txn,
                            step.search_path,
                            ObjectKind::Entity,
                            name,
                        )
                        .await?;
                        let key = owner_key_type(step, &stored)?;
                        (Some(stored), Some(key))
                    }
                };
                let settings = Settings {
                    namespace: namespace.to_owned(),
                    owner_type,
                    owner_key_type,
                    quota_bytes: options.quota_bytes,
                    quota_files: options.quota_files,
                };
                settings::store(step.txn, &settings).await?;
                Ok(settings)
            })
            .await?;
        let (before, after) = file_schema(&settings);
        for statement in &before {
            self.run_ddl(statement).await?;
        }
        let owner = owner_text(&self.context().principal);
        let table = catalog::stored_name(namespace, "fs_nodes");
        // The root is written directly: the table has just been made and
        // isn't governed yet, and the active policies may name the entity
        // type this is about to declare.
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let nodes = interlaken_storage::catalog::table(step.txn, &table)
                .await?
                .ok_or_else(|| EngineError::Catalog(format!("{table} was not created")))?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
            interlaken_storage::rows::insert(
                step.txn,
                &nodes,
                vec![
                    Stored::Text(ROOT_ID.into()),
                    Stored::Null,
                    Stored::Text(String::new()),
                    Stored::Text(FileKind::Dir.as_str().into()),
                    Stored::Text(owner.clone()),
                    Stored::Null,
                    Stored::Int8(0),
                    Stored::Null,
                    Stored::Null,
                    Stored::Timestamptz(now),
                    Stored::Timestamptz(now),
                ],
            )
            .await?;
            Ok(())
        })
        .await?;
        for statement in &after {
            self.run_ddl(statement).await?;
        }
        Ok(())
    }

    /// Return namespace `namespace`'s files, which [`Session::enable_files`]
    /// must have set up.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::InvalidDefinition`] if the namespace has no
    /// files, or a storage error.
    pub async fn files(&mut self, namespace: &str) -> Result<Files<'_>, EngineError> {
        let found = self
            .run(Kind::Query, async |step: &Step<'_>| {
                settings::load(step.txn, namespace).await
            })
            .await?;
        let settings = found.ok_or_else(|| {
            EngineError::InvalidDefinition(format!("namespace {namespace} has no files"))
        })?;
        Ok(Files {
            session: self,
            settings,
        })
    }

    /// Set namespace `namespace`'s file quotas: the most bytes of file
    /// content and the most files and directories it may hold (`None` for
    /// no limit). Usage already over a new quota stays, but can't grow.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::PermissionDenied`] without `admin` or
    /// ownership of the namespace, or [`EngineError::InvalidDefinition`] if
    /// it has no files.
    pub async fn set_file_quota(
        &mut self,
        namespace: &str,
        bytes: Option<u64>,
        files: Option<u64>,
    ) -> Result<(), EngineError> {
        self.run(Kind::Admin, async |step: &Step<'_>| {
            may_change(step, &[namespace]).await?;
            settings::set_quota(step.txn, namespace, bytes, files).await
        })
        .await
    }
}

/// Map an error from writing a node at `path` to a file system error.
fn file_error(path: &str, error: EngineError) -> EngineError {
    match error.sqlstate() {
        "23505" => EngineError::FileExists(path.to_owned()),
        "23503" => EngineError::DirectoryNotEmpty(path.to_owned()),
        "23514" => {
            EngineError::InvalidPath(format!("{path:?}: a directory can't be moved into itself"))
        }
        _ => error,
    }
}

/// Run `$body`, an operation of `$files`, in the session's transaction, or
/// in an implicit one committed if it succeeds, so its metadata and usage
/// changes apply together.
macro_rules! atomically {
    ($files:ident, $body:expr) => {{
        let implicit = !$files.session.in_transaction();
        if implicit {
            $files.session.begin(TxnOptions::default()).await?;
        }
        let result = $body;
        $files.end_operation(implicit, result).await
    }};
}

impl Files<'_> {
    /// End an operation [`atomically`] began: commit or roll back its
    /// implicit transaction.
    async fn end_operation<T>(
        &mut self,
        implicit: bool,
        result: Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        if !implicit {
            return result;
        }
        match result {
            Ok(value) => self.session.commit().await.map(|_| value),
            Err(e) => {
                if self.session.in_transaction() {
                    self.session.rollback()?;
                }
                Err(e)
            }
        }
    }

    /// Change the namespace's usage by `files` and `bytes`.
    async fn adjust_usage(&mut self, files: i64, bytes: i64) -> Result<(), EngineError> {
        let namespace = self.settings.namespace.clone();
        self.session
            .run(Kind::Query, async |step: &Step<'_>| {
                settings::adjust_usage(step.txn, &namespace, files, bytes).await
            })
            .await
    }

    /// Return what the namespace's files use, and may use.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn usage(&mut self) -> Result<FileUsage, EngineError> {
        let namespace = self.settings.namespace.clone();
        self.session
            .run(Kind::Query, async |step: &Step<'_>| {
                settings::usage(step.txn, &namespace).await
            })
            .await
    }

    /// Return the qualified name of the nodes table.
    fn nodes(&self) -> String {
        object(&self.settings.namespace, "fs_nodes")
    }

    /// Return the namespace these files are in.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.settings.namespace
    }

    /// Return the nodes the plan `filter` (over alias `n`, with parameter
    /// `$1` of type text) selects for `value`.
    async fn select(&mut self, filter: &str, value: &str) -> Result<Vec<FileMeta>, EngineError> {
        let text = format!(
            "(plan (project (filter (get {} :as n) {filter}) {META_COLUMNS}))",
            self.nodes()
        );
        let result = self
            .session
            .query(&text, vec![Datum::Text(value.to_owned())])
            .await?;
        result
            .rows()
            .iter()
            .map(|r| FileMeta::from_row(r))
            .collect()
    }

    /// Return node `id`, if the principal may read it.
    async fn node(&mut self, id: &str) -> Result<Option<FileMeta>, EngineError> {
        Ok(self
            .select("(= n.id (param 1 text))", id)
            .await?
            .into_iter()
            .next())
    }

    /// Return the child of directory `parent` named `name`, if the
    /// principal may read it.
    async fn child(&mut self, parent: &str, name: &str) -> Result<Option<FileMeta>, EngineError> {
        let text = format!(
            "(plan (project (filter (get {} :as n) (and (= n.parent (param 1 text)) (= n.name (param 2 text)))) {META_COLUMNS}))",
            self.nodes()
        );
        let result = self
            .session
            .query(
                &text,
                vec![Datum::Text(parent.to_owned()), Datum::Text(name.to_owned())],
            )
            .await?;
        result
            .rows()
            .iter()
            .map(|r| FileMeta::from_row(r))
            .next()
            .transpose()
    }

    /// Return the node at `names` below the root, walking one directory at
    /// a time; `None` if any step is missing or hidden from the principal.
    async fn walk(&mut self, names: &[String]) -> Result<Option<FileMeta>, EngineError> {
        let Some(mut node) = self.node(ROOT_ID).await? else {
            return Ok(None);
        };
        for name in names {
            if !node.is_dir() {
                return Ok(None);
            }
            match self.child(&node.id, name).await? {
                Some(next) => node = next,
                None => return Ok(None),
            }
        }
        Ok(Some(node))
    }

    /// Return the node at `path`.
    async fn resolve(&mut self, path: &str) -> Result<FileMeta, EngineError> {
        let names = split(path)?;
        self.walk(&names)
            .await?
            .ok_or_else(|| EngineError::FileNotFound(path.to_owned()))
    }

    /// Return the directory that holds `path`, and `path`'s name in it.
    async fn parent_of(&mut self, path: &str) -> Result<(FileMeta, String), EngineError> {
        let (names, name) = split_last(path)?;
        let parent = self
            .walk(&names)
            .await?
            .ok_or_else(|| EngineError::FileNotFound(path.to_owned()))?;
        if !parent.is_dir() {
            return Err(EngineError::WrongFileKind(format!(
                "{path}: its parent isn't a directory"
            )));
        }
        Ok((parent, name))
    }

    /// Return the value of `owner_id` for `principal`: its key if it is of
    /// the owner type, NULL otherwise.
    fn owner_id(&self, principal: &EntityUid) -> Datum {
        match (&self.settings.owner_type, &principal.key) {
            (Some(ty), key) if ty == principal.ty.as_str() => match key {
                EntityKey::Long(k) => match self.settings.owner_key_type.as_deref() {
                    Some("int2") => i16::try_from(*k).map_or(Datum::Null, Datum::Int2),
                    Some("int4") => i32::try_from(*k).map_or(Datum::Null, Datum::Int4),
                    Some("int8") => Datum::Int8(*k),
                    _ => Datum::Text(k.to_string()),
                },
                EntityKey::Str(k) => match self.settings.owner_key_type.as_deref() {
                    Some("text") | None => Datum::Text(k.clone()),
                    _ => Datum::Null,
                },
            },
            _ => Datum::Null,
        }
    }

    /// Insert a node's row, with a generated id, and return its metadata.
    async fn insert_node(
        &mut self,
        parent: &str,
        name: &str,
        kind: FileKind,
        owner: &EntityUid,
        content: Option<(u64, String, Vec<String>)>,
    ) -> Result<FileMeta, EngineError> {
        let owner_key = self.settings.owner_key_type.as_deref().unwrap_or("text");
        let text = format!(
            "(plan (insert {nodes} (values (columns a b c d e f g h i j k) \
               (row (cast (call gen_random_uuid) text) (param 1 text) (param 2 text) (param 3 text) \
                    (param 4 text) (param 5 (option {owner_key})) (param 6 int8) \
                    (param 7 (option text)) (param 8 (option (array text))) (call now) (call now))) \
             (columns id parent name kind owner owner_id size hash chunks created modified) \
             (returning id)))",
            nodes = self.nodes()
        );
        let (size, hash, chunk_list) = match content {
            Some((size, hash, chunks)) => (
                i64::try_from(size).unwrap_or(i64::MAX),
                Datum::Text(hash),
                Datum::Array(chunks.into_iter().map(Datum::Text).collect()),
            ),
            None => (0, Datum::Null, Datum::Null),
        };
        let params = vec![
            Datum::Text(parent.to_owned()),
            Datum::Text(name.to_owned()),
            Datum::Text(kind.as_str().to_owned()),
            Datum::Text(owner_text(owner)),
            self.owner_id(owner),
            Datum::Int8(size),
            hash,
            chunk_list,
        ];
        let result = self.session.query(&text, params).await?;
        self.adjust_usage(1, size).await?;
        let new_id = match result.rows().first().and_then(|r| r.first()) {
            Some(Datum::Text(id)) => id.clone(),
            _ => return Err(EngineError::Catalog("insert returned no id".into())),
        };
        self.node(&new_id)
            .await?
            .ok_or_else(|| EngineError::FileNotFound(name.to_owned()))
    }

    /// Run the DML `text` on node `id` with parameters after `$1`, and
    /// require it to change that node.
    async fn change_node(
        &mut self,
        path: &str,
        text: &str,
        id: &str,
        mut params: Vec<Datum>,
    ) -> Result<(), EngineError> {
        params.insert(0, Datum::Text(id.to_owned()));
        let result = self
            .session
            .query(text, params)
            .await
            .map_err(|e| file_error(path, e))?;
        if result.row_count() == 0 {
            return Err(EngineError::FileNotFound(path.to_owned()));
        }
        Ok(())
    }

    /// Return the metadata of the file or directory at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::FileNotFound`] (`58P01`) if it doesn't exist
    /// or the principal may not read it or a directory above it, or
    /// [`EngineError::InvalidPath`].
    pub async fn stat(&mut self, path: &str) -> Result<FileMeta, EngineError> {
        self.resolve(path).await
    }

    /// Return `true` if `path` exists and the principal may read it.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::InvalidPath`] or a storage error.
    pub async fn exists(&mut self, path: &str) -> Result<bool, EngineError> {
        Ok(self.walk(&split(path)?).await?.is_some())
    }

    /// Return the entries of directory `path` the principal may read, by name.
    ///
    /// # Errors
    ///
    /// As for [`Files::stat`], and [`EngineError::WrongFileKind`] (`42809`)
    /// for a file.
    pub async fn read_dir(&mut self, path: &str) -> Result<Vec<FileMeta>, EngineError> {
        let dir = self.resolve(path).await?;
        if !dir.is_dir() {
            return Err(EngineError::WrongFileKind(format!(
                "{path} isn't a directory"
            )));
        }
        let mut entries = self.select("(= n.parent (param 1 text))", &dir.id).await?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Create directory `path`, whose parent must exist.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::FileExists`] (`58P02`) if the name is taken,
    /// [`EngineError::FileNotFound`] for a missing parent, or `42501` if
    /// policies don't permit inserting it.
    pub async fn create_dir(&mut self, path: &str) -> Result<FileMeta, EngineError> {
        atomically!(self, self.create_dir_in(path).await)
    }

    /// Run [`Files::create_dir`] in the open transaction.
    async fn create_dir_in(&mut self, path: &str) -> Result<FileMeta, EngineError> {
        let (parent, name) = self.parent_of(path).await?;
        let owner = self.session.context().principal.clone();
        self.insert_node(&parent.id, &name, FileKind::Dir, &owner, None)
            .await
            .map_err(|e| file_error(path, e))
    }

    /// Create directory `path` and any missing directories above it.
    ///
    /// # Errors
    ///
    /// As for [`Files::create_dir`], and [`EngineError::WrongFileKind`] if
    /// a file is in the way.
    pub async fn create_dir_all(&mut self, path: &str) -> Result<FileMeta, EngineError> {
        atomically!(self, self.create_dir_all_in(path).await)
    }

    /// Run [`Files::create_dir_all`] in the open transaction.
    async fn create_dir_all_in(&mut self, path: &str) -> Result<FileMeta, EngineError> {
        let names = split(path)?;
        let mut node = self.resolve("/").await?;
        let owner = self.session.context().principal.clone();
        for name in &names {
            if !node.is_dir() {
                return Err(EngineError::WrongFileKind(format!(
                    "{path}: {} isn't a directory",
                    node.name
                )));
            }
            node = match self.child(&node.id, name).await? {
                Some(next) => next,
                None => self
                    .insert_node(&node.id, name, FileKind::Dir, &owner, None)
                    .await
                    .map_err(|e| file_error(path, e))?,
            };
        }
        Ok(node)
    }

    /// Start writing file `path`, replacing it if it exists. Content is
    /// staged as it is written; the file appears or changes only when
    /// [`FileWriter::finish`] records it in the session's transaction.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::FileNotFound`] for a missing parent, or
    /// [`EngineError::WrongFileKind`] if `path` is a directory.
    pub async fn create(&mut self, path: &str) -> Result<FileWriter<'_>, EngineError> {
        let (parent, name) = self.parent_of(path).await?;
        let existing = self.child(&parent.id, &name).await?;
        if existing.as_ref().is_some_and(FileMeta::is_dir) {
            return Err(EngineError::WrongFileKind(format!("{path} is a directory")));
        }
        let existing = existing.map(|e| (e.id, e.size));
        let usage = self.usage().await?;
        let limit = usage.quota_bytes.map(|quota| {
            quota.saturating_sub(usage.bytes) + existing.as_ref().map_or(0, |(_, size)| *size)
        });
        let storage = self.session.storage();
        Ok(FileWriter::new(
            self,
            storage,
            path.to_owned(),
            parent.id,
            name,
            existing,
            limit,
        ))
    }

    /// Record a written file: insert its row, or update `existing`'s (its
    /// id and old size).
    pub(crate) async fn record_file(
        &mut self,
        path: &str,
        parent: &str,
        name: &str,
        existing: Option<(&str, u64)>,
        content: (u64, String, Vec<String>),
    ) -> Result<FileMeta, EngineError> {
        atomically!(
            self,
            self.record_file_in(path, parent, name, existing, content)
                .await
        )
    }

    /// Run [`Files::record_file`] in the open transaction.
    async fn record_file_in(
        &mut self,
        path: &str,
        parent: &str,
        name: &str,
        existing: Option<(&str, u64)>,
        content: (u64, String, Vec<String>),
    ) -> Result<FileMeta, EngineError> {
        let Some((id, old_size)) = existing else {
            let owner = self.session.context().principal.clone();
            return self
                .insert_node(parent, name, FileKind::File, &owner, Some(content))
                .await
                .map_err(|e| file_error(path, e));
        };
        let (size, hash, chunk_list) = content;
        let grown = i64::try_from(size).unwrap_or(i64::MAX) - i64::try_from(old_size).unwrap_or(0);
        self.adjust_usage(0, grown).await?;
        let text = format!(
            "(plan (update {nodes} (filter (get {nodes} :as n) (= n.id (param 1 text))) \
               (set size (param 2 int8)) (set hash (param 3 (option text))) \
               (set chunks (param 4 (option (array text)))) (set modified (call now)) \
               (returning id)))",
            nodes = self.nodes()
        );
        self.change_node(
            path,
            &text,
            id,
            vec![
                Datum::Int8(i64::try_from(size).unwrap_or(i64::MAX)),
                Datum::Text(hash),
                Datum::Array(chunk_list.into_iter().map(Datum::Text).collect()),
            ],
        )
        .await?;
        self.node(id)
            .await?
            .ok_or_else(|| EngineError::FileNotFound(path.to_owned()))
    }

    /// Write `content` to file `path` whole, replacing it if it exists.
    ///
    /// # Errors
    ///
    /// As for [`Files::create`] and [`FileWriter::finish`].
    pub async fn write(&mut self, path: &str, content: &[u8]) -> Result<FileMeta, EngineError> {
        let mut writer = self.create(path).await?;
        writer.write_bytes(content).await?;
        writer.finish().await
    }

    /// Open file `path` for reading. The reader streams its content as of
    /// the session's snapshot, checking each chunk against its hash.
    ///
    /// # Errors
    ///
    /// As for [`Files::stat`], and [`EngineError::WrongFileKind`] for a
    /// directory.
    pub async fn open(&mut self, path: &str) -> Result<FileReader, EngineError> {
        let meta = self.resolve(path).await?;
        if meta.is_dir() {
            return Err(EngineError::WrongFileKind(format!("{path} is a directory")));
        }
        Ok(FileReader::new(
            self.session.storage(),
            self.settings.namespace.clone(),
            meta,
        ))
    }

    /// Return the whole content of file `path`.
    ///
    /// # Errors
    ///
    /// As for [`Files::open`], and [`EngineError::FileCorrupt`] for a chunk
    /// that is missing or doesn't match its hash.
    pub async fn read(&mut self, path: &str) -> Result<Vec<u8>, EngineError> {
        self.open(path).await?.read_all().await
    }

    /// Move or rename `from` to `to`, whose parent must exist; a directory
    /// moves with everything in it.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::FileNotFound`], [`EngineError::FileExists`]
    /// if `to` is taken, [`EngineError::InvalidPath`] for moving a
    /// directory into itself or moving the root, or `42501` if policies
    /// don't permit updating the node.
    pub async fn rename(&mut self, from: &str, to: &str) -> Result<FileMeta, EngineError> {
        atomically!(self, self.rename_in(from, to).await)
    }

    /// Run [`Files::rename`] in the open transaction.
    async fn rename_in(&mut self, from: &str, to: &str) -> Result<FileMeta, EngineError> {
        let node = self.resolve(from).await?;
        if node.id == ROOT_ID {
            return Err(EngineError::InvalidPath("the root can't be moved".into()));
        }
        let (parent, name) = self.parent_of(to).await?;
        let text = format!(
            "(plan (update {nodes} (filter (get {nodes} :as n) (= n.id (param 1 text))) \
               (set parent (param 2 (option text))) (set name (param 3 text)) \
               (set modified (call now)) (returning id)))",
            nodes = self.nodes()
        );
        self.change_node(
            to,
            &text,
            &node.id,
            vec![Datum::Text(parent.id), Datum::Text(name)],
        )
        .await?;
        self.node(&node.id)
            .await?
            .ok_or_else(|| EngineError::FileNotFound(to.to_owned()))
    }

    /// Remove the file or empty directory at `path`. Its content stays
    /// until collection finds no file refers to it.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::FileNotFound`],
    /// [`EngineError::DirectoryNotEmpty`] (`2BP01`) for a directory holding
    /// anything (including entries the principal can't see),
    /// [`EngineError::InvalidPath`] for the root, or `42501` if policies
    /// don't permit deleting it.
    pub async fn remove(&mut self, path: &str) -> Result<(), EngineError> {
        atomically!(self, self.remove_in(path).await)
    }

    /// Run [`Files::remove`] in the open transaction.
    async fn remove_in(&mut self, path: &str) -> Result<(), EngineError> {
        let node = self.resolve(path).await?;
        if node.id == ROOT_ID {
            return Err(EngineError::InvalidPath("the root can't be removed".into()));
        }
        let text = format!(
            "(plan (delete {nodes} (filter (get {nodes} :as n) (= n.id (param 1 text))) (returning id)))",
            nodes = self.nodes()
        );
        self.change_node(path, &text, &node.id, Vec::new()).await?;
        let size = i64::try_from(node.size).unwrap_or(i64::MAX);
        self.adjust_usage(-1, -size).await
    }
}
