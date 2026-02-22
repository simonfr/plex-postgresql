pub mod dedup;
pub mod emit;
pub mod ffi;
pub mod functions;
pub mod groupby;
pub mod keywords;
pub mod pg_client;
pub mod pg_config;
pub mod pg_logging;
pub mod pg_mem_telemetry;
pub mod pg_query_cache;
pub mod pg_statement;
/// sql-translator: SQLite → PostgreSQL SQL translation using sqlparser-rs AST
///
/// Pipeline (in order):
///   1. placeholders  — ? / :name → $1, $2, ...
///   2. functions     — IFNULL→COALESCE, iif→CASE, typeof→pg_typeof, etc.
///   3. types         — AUTOINCREMENT, BLOB, INTEGER8, etc.
///   4. keywords      — GLOB→ILIKE, BEGIN IMMEDIATE, sqlite_master, etc.
///   5. upsert        — INSERT OR REPLACE / INSERT OR IGNORE → ON CONFLICT
///   6. quotes        — backtick identifiers → double-quote
///   7. groupby       — GROUP BY strict mode (add missing non-aggregate cols)
///   8. query         — misc query fixups (subquery alias, NULLS FIRST, etc.)
pub mod placeholders;
pub mod query;
pub mod quotes;
pub mod shim_alloc;
pub mod types;
pub mod upsert;

use sqlparser::dialect::SQLiteDialect;
use sqlparser::parser::Parser;

/// Result of a full translation
#[derive(Debug, Clone)]
pub struct Translation {
    /// Translated PostgreSQL SQL
    pub sql: String,
    /// Original named parameter names in bind order (None for ? placeholders)
    pub param_names: Vec<Option<String>>,
}

/// Translate SQLite SQL to PostgreSQL SQL.
/// Returns Err with a description on parse or translation failure.
pub fn translate(sqlite_sql: &str) -> Result<Translation, String> {
    if sqlite_sql.is_empty() {
        return Ok(Translation {
            sql: String::new(),
            param_names: vec![],
        });
    }

    // Pre-parse normalisation: handle constructs sqlparser doesn't support
    // (GLOB → ILIKE, INDEXED BY → removed)
    let preprocessed = keywords::preprocess(sqlite_sql);

    let dialect = SQLiteDialect {};
    let mut stmts =
        Parser::parse_sql(&dialect, &preprocessed).map_err(|e| format!("parse error: {e}"))?;

    let mut param_names: Vec<Option<String>> = Vec::new();

    for stmt in &mut stmts {
        placeholders::transform(stmt, &mut param_names);
        functions::transform(stmt);
        types::transform(stmt);
        keywords::transform(stmt);
        upsert::transform(stmt);
        quotes::transform(stmt);
        groupby::transform(stmt);
        query::transform(stmt);
        dedup::transform(stmt);
    }

    let sql = stmts
        .iter()
        .map(|s| emit::emit(s))
        .collect::<Vec<_>>()
        .join("; ");

    Ok(Translation { sql, param_names })
}
