#![allow(
    clippy::cmp_null,
    clippy::collapsible_match,
    clippy::collapsible_else_if,
    clippy::collapsible_if,
    clippy::collapsible_str_replace,
    clippy::doc_lazy_continuation,
    clippy::doc_overindented_list_items,
    clippy::explicit_counter_loop,
    clippy::if_same_then_else,
    clippy::manual_c_str_literals,
    clippy::manual_is_ascii_check,
    clippy::manual_pattern_char_comparison,
    clippy::missing_const_for_thread_local,
    clippy::needless_range_loop,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::ptr_eq,
    clippy::single_match,
    clippy::too_many_arguments,
    clippy::wildcard_in_or_patterns,
)]

pub mod dedup;
pub mod byte_utils;
pub mod db_interpose_exec;
pub mod c_abi;
pub mod db_interpose_bind;
pub mod db_interpose_column;
pub mod db_interpose_common;
pub mod exception_what;
pub mod db_interpose_open;
pub mod db_interpose_prepare;
pub mod db_interpose_stmt_lifecycle;
pub mod db_interpose_prepare_utils;
pub mod db_interpose_value;
pub mod db_interpose_helpers;
pub mod db_interpose_metadata;
pub mod db_interpose_prepare_helpers;
pub mod db_interpose_trace_helpers;
pub mod db_interpose_conn_utils;
pub mod db_interpose_step_cached_read_utils;
pub mod db_interpose_step_read_utils;
pub mod db_interpose_step;
pub mod db_interpose_step_write_utils;
pub mod db_interpose_txn_utils;
pub mod db_interpose_value_helpers;
pub mod env_utils;
pub mod emit;
pub mod ffi_types;
pub mod ffi;
pub mod functions;
pub mod groupby;
pub mod keywords;
pub mod libpq_helpers;
pub mod pg_client;
pub mod pg_client_stmt_cache;
pub mod pg_config;
pub mod pg_logging;
pub mod pg_mem_telemetry;
pub mod pg_query_cache;
pub mod pg_statement;
pub mod platform_backtrace;
pub mod runtime_common;
#[cfg(target_os = "macos")]
pub mod fishhook;
#[cfg(target_os = "macos")]
pub mod runtime_macos;
#[cfg(target_os = "linux")]
pub mod runtime_linux;
/// plex-pg-core: SQLite → PostgreSQL SQL translation, caching, statement lifecycle & more
///
/// Pipeline (in order):
///   1. dedup         — UPDATE duplicate SET targets (keep last assignment)
///   2. placeholders  — ? / :name → $1, $2, ...
///   3. functions     — IFNULL→COALESCE, iif→CASE, typeof→pg_typeof, etc.
///   4. types         — AUTOINCREMENT, BLOB, INTEGER8, etc.
///   5. keywords      — GLOB→ILIKE, BEGIN IMMEDIATE, sqlite_master, etc.
///   6. upsert        — INSERT OR REPLACE / INSERT OR IGNORE → ON CONFLICT
///   7. quotes        — backtick identifiers → double-quote
///   8. groupby       — GROUP BY strict mode (add missing non-aggregate cols)
///   9. query         — misc query fixups (subquery alias, NULLS FIRST, etc.)
pub mod placeholders;
pub mod query;
pub mod rewriter;
pub mod quotes;
pub mod shim_alloc;
mod sync_utils;
pub mod types;
pub mod upsert;
#[cfg(test)]
pub mod test_utils;

use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::parser::Parser;

/// Result of a full translation
#[derive(Debug, Clone)]
pub struct Translation {
    /// Translated PostgreSQL SQL
    pub sql: String,
    /// Original named parameter names in bind order (None for ? placeholders)
    pub param_names: Vec<Option<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputValidationMode {
    Off,
    Sample,
    All,
}

const VALIDATE_MODE_ENV: &str = "PLEX_PG_VALIDATE_OUTPUT";
const VALIDATE_SAMPLE_PCT_ENV: &str = "PLEX_PG_VALIDATE_OUTPUT_SAMPLE_PCT";
const DEFAULT_VALIDATE_SAMPLE_PCT: u8 = 5;

fn parse_output_validation_mode(raw: Option<&str>) -> OutputValidationMode {
    match raw.unwrap_or("off").trim().to_ascii_lowercase().as_str() {
        "all" => OutputValidationMode::All,
        "sample" => OutputValidationMode::Sample,
        _ => OutputValidationMode::Off,
    }
}

fn parse_sample_pct(raw: Option<&str>) -> u8 {
    raw.and_then(|s| s.trim().parse::<u8>().ok())
        .map(|v| v.min(100))
        .unwrap_or(DEFAULT_VALIDATE_SAMPLE_PCT)
}

fn stable_hash64(input: &str) -> u64 {
    // FNV-1a (64-bit), deterministic and fast for sampling decisions.
    let mut hash = 0xcbf29ce484222325u64;
    for b in input.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn should_validate_output(mode: OutputValidationMode, translated_sql: &str, sample_pct: u8) -> bool {
    match mode {
        OutputValidationMode::Off => false,
        OutputValidationMode::All => true,
        OutputValidationMode::Sample => {
            if sample_pct == 0 {
                return false;
            }
            if sample_pct >= 100 {
                return true;
            }
            (stable_hash64(translated_sql) % 100) < u64::from(sample_pct)
        }
    }
}

fn validate_postgres_output(sql: &str) -> Result<(), String> {
    let dialect = PostgreSqlDialect {};
    Parser::parse_sql(&dialect, sql)
        .map(|_| ())
        .map_err(|e| format!("postgres validation error: {e}"))
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
    if preprocessed.trim().is_empty() {
        return Ok(Translation {
            sql: String::new(),
            param_names: vec![],
        });
    }

    let sqlite_dialect = SQLiteDialect {};
    let mut stmts = match Parser::parse_sql(&sqlite_dialect, &preprocessed) {
        Ok(stmts) => stmts,
        Err(sqlite_err) => {
            // sqlparser's SQLiteDialect rejects some valid SQLite/MySQL-style
            // backtick-qualified forms (`alias.`reserved``). In that case,
            // retry with MySQL dialect to keep translation coverage broad.
            if preprocessed.as_bytes().contains(&b'`') {
                let mysql_dialect = MySqlDialect {};
                match Parser::parse_sql(&mysql_dialect, &preprocessed) {
                    Ok(stmts) => stmts,
                    Err(mysql_err) => {
                        return Err(format!(
                            "parse error: {sqlite_err}; mysql fallback parse error: {mysql_err}"
                        ));
                    }
                }
            } else {
                return Err(format!("parse error: {sqlite_err}"));
            }
        }
    };

    let mut param_names: Vec<Option<String>> = Vec::new();

    let pipeline = rewriter::pipeline::RewritePipeline::default();
    for stmt in &mut stmts {
        let mut ctx = rewriter::rules::RewriteContext {
            param_names: &mut param_names,
        };
        pipeline.apply(stmt, &mut ctx);
    }

    let sql = stmts
        .iter()
        .map(emit::emit)
        .collect::<Vec<_>>()
        .join("; ");

    let validation_mode = parse_output_validation_mode(env_utils::env_string(VALIDATE_MODE_ENV).as_deref());
    let sample_pct = parse_sample_pct(env_utils::env_string(VALIDATE_SAMPLE_PCT_ENV).as_deref());
    if should_validate_output(validation_mode, &sql, sample_pct) {
        validate_postgres_output(&sql)?;
    }

    Ok(Translation { sql, param_names })
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn validation_output__output_validation_mode_defaults_to_off() {
        assert_eq!(parse_output_validation_mode(None), OutputValidationMode::Off);
        assert_eq!(
            parse_output_validation_mode(Some("")),
            OutputValidationMode::Off
        );
        assert_eq!(
            parse_output_validation_mode(Some("invalid")),
            OutputValidationMode::Off
        );
    }

    #[test]
    fn validation_output__output_validation_mode_parses_values() {
        assert_eq!(
            parse_output_validation_mode(Some("off")),
            OutputValidationMode::Off
        );
        assert_eq!(
            parse_output_validation_mode(Some("sample")),
            OutputValidationMode::Sample
        );
        assert_eq!(
            parse_output_validation_mode(Some("all")),
            OutputValidationMode::All
        );
        assert_eq!(
            parse_output_validation_mode(Some("  ALL  ")),
            OutputValidationMode::All
        );
    }

    #[test]
    fn validation_output__output_validation_sample_pct_defaults_and_bounds() {
        assert_eq!(parse_sample_pct(None), DEFAULT_VALIDATE_SAMPLE_PCT);
        assert_eq!(parse_sample_pct(Some("")), DEFAULT_VALIDATE_SAMPLE_PCT);
        assert_eq!(parse_sample_pct(Some("7")), 7);
        assert_eq!(parse_sample_pct(Some("255")), 100);
        assert_eq!(parse_sample_pct(Some("bad")), DEFAULT_VALIDATE_SAMPLE_PCT);
    }

    #[test]
    fn validation_output__output_validation_sampling_decision_respects_mode() {
        let sql = "SELECT 1";
        assert!(!should_validate_output(OutputValidationMode::Off, sql, 100));
        assert!(should_validate_output(OutputValidationMode::All, sql, 0));
        assert!(!should_validate_output(OutputValidationMode::Sample, sql, 0));
        assert!(should_validate_output(OutputValidationMode::Sample, sql, 100));
    }

    #[test]
    fn validation_output__output_validation_postgres_parser_accepts_valid_sql() {
        assert!(validate_postgres_output("SELECT 1").is_ok());
    }
}
