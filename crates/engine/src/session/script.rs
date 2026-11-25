//! DDL statements as text, and scripts of statements (ADR-0043).

use interlaken_ir::ddl::{DdlStatement, Statement};
use interlaken_ir::interp::Datum;
use interlaken_ir::text::{ScriptStatement, parse_script};
use interlaken_storage::TxnOptions;

use super::connection::Session;
use super::result::QueryResult;
use crate::error::EngineError;

/// What one statement of a script did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptEntry {
    /// Its head word: `plan`, or a DDL form such as `create-table`.
    pub form: &'static str,
    /// The line it starts on.
    pub line: u32,
    /// The column it starts at.
    pub column: u32,
    /// The rows it returned; 0 for DDL.
    pub rows: usize,
}

/// What a script did, statement by statement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptReport {
    /// Each statement, in order.
    pub statements: Vec<ScriptEntry>,
    /// The commit timestamp of the script's implicit transaction, if it
    /// ran in one and wrote something; `None` in an open transaction.
    pub commit_ts: Option<crate::CommitTs>,
}

impl Session {
    /// Run one DDL statement, as the Rust operation it names does.
    ///
    /// # Errors
    ///
    /// The operation's errors: [`EngineError::PermissionDenied`] without
    /// the privilege or ownership it needs, or the definition's error.
    pub async fn run_ddl(&mut self, statement: &DdlStatement) -> Result<(), EngineError> {
        match statement {
            DdlStatement::CreateTable(spec) => self.create_table(spec).await,
            DdlStatement::DropTable(name) => self.drop_table(name).await,
            DdlStatement::AddColumn { table, column } => self.add_column(table, column).await,
            DdlStatement::CreateIndex(spec) => self.create_index(spec).await,
            DdlStatement::DropIndex(name) => self.drop_index(name).await,
            DdlStatement::DeclareEntity(spec) => self.declare_entity(spec).await,
            DdlStatement::DropEntity(name) => self.drop_entity(name).await,
            DdlStatement::DeclarePrincipalType(name) => self.declare_principal_type(name).await,
            DdlStatement::DropPrincipalType(name) => self.drop_principal_type(name).await,
            DdlStatement::DeclareMembership(spec) => self.declare_membership(spec).await,
            DdlStatement::DropMembership { member, group } => {
                self.drop_membership(member, group).await
            }
            DdlStatement::CreateForeignKey(spec) => self.create_foreign_key(spec).await,
            DdlStatement::DropForeignKey(name) => self.drop_foreign_key(name).await,
            DdlStatement::SetLeakproof {
                function,
                args,
                leakproof,
            } => {
                self.set_function_leakproof(function, args, *leakproof)
                    .await
            }
            DdlStatement::CreateNamespace { name, owner } => {
                self.create_namespace(name, owner).await
            }
            DdlStatement::DropNamespace { name, cascade } => {
                self.drop_namespace(name, *cascade).await
            }
            DdlStatement::SetNamespaceOwner { name, owner } => {
                self.set_namespace_owner(name, owner).await
            }
        }
    }

    /// Run a DDL statement given to [`Session::query`], which takes no
    /// parameters and returns no columns.
    pub(super) async fn query_ddl(
        &mut self,
        statement: &DdlStatement,
        params: &[Datum],
    ) -> Result<QueryResult, EngineError> {
        if !params.is_empty() {
            return Err(EngineError::Parameter(
                interlaken_ir::interp::ParamError::Count {
                    expected: 0,
                    supplied: params.len(),
                },
            ));
        }
        self.run_ddl(statement).await?;
        Ok(QueryResult {
            columns: Vec::new(),
            types: Vec::new(),
            batches: Vec::new(),
            usage: super::Usage::default(),
            commit_ts: None,
        })
    }

    /// Run `item`, one statement of a script, in the open transaction.
    async fn run_script_statement(
        &mut self,
        item: &ScriptStatement,
    ) -> Result<ScriptEntry, EngineError> {
        let (form, rows) = match &item.statement {
            Statement::Plan(plan) => ("plan", self.query_plan(plan, Vec::new()).await?.row_count()),
            Statement::Ddl(ddl) => {
                self.run_ddl(ddl).await?;
                (ddl.form(), 0)
            }
        };
        Ok(ScriptEntry {
            form,
            line: item.line,
            column: item.column,
            rows,
        })
    }

    /// Run a script: statements of IR text, `(plan ...)` and DDL mixed, in
    /// order, with `;` comments (ADR-0043).
    ///
    /// In an open transaction the statements run in it, and a failing
    /// statement is undone and aborts the transaction as any statement does
    /// (ADR-0026), leaving the caller to roll back. Otherwise the script runs
    /// in one implicit transaction, so it applies entirely or not at all.
    /// Nothing records which scripts have run: a migration tool records that
    /// in its own table, in the same transaction.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Syntax`] if the script doesn't parse, before
    /// anything runs; [`EngineError::Script`] naming the statement that
    /// failed and why; or the commit's error.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run(session: &mut interlaken_engine::Session) -> Result<(), interlaken_engine::EngineError> {
    /// let report = session
    ///     .execute_script(
    ///         "(create-table todos (column id int8) (column title text) (primary-key id))
    ///          (plan (insert todos (values (columns a b) (row 1 \"first\")) (columns id title)))",
    ///     )
    ///     .await?;
    /// assert_eq!(report.statements.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute_script(&mut self, text: &str) -> Result<ScriptReport, EngineError> {
        let script = parse_script(text).map_err(EngineError::Syntax)?;
        let implicit = !self.in_transaction();
        if implicit {
            self.begin(TxnOptions::default()).await?;
        }
        let mut report = ScriptReport::default();
        for (index, item) in script.iter().enumerate() {
            match self.run_script_statement(item).await {
                Ok(entry) => report.statements.push(entry),
                Err(source) => {
                    if implicit && self.in_transaction() {
                        self.rollback()?;
                    }
                    return Err(EngineError::Script {
                        index: index + 1,
                        form: match &item.statement {
                            Statement::Plan(_) => "plan".into(),
                            Statement::Ddl(ddl) => ddl.form().into(),
                        },
                        line: item.line,
                        column: item.column,
                        source: Box::new(source),
                    });
                }
            }
        }
        if implicit {
            report.commit_ts = self.commit().await?;
        }
        Ok(report)
    }
}
