
#![allow(clippy::enum_variant_names)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use postgres::{Client, NoTls, SimpleQueryMessage};
use regex::Regex;
use rusqlite::{types::ValueRef, Connection};

use plex_pg_core::translate;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortMode {
    NoSort,
    RowSort,
    ValueSort,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatementExpectation {
    Ok,
    Error,
}

#[derive(Debug)]
enum CaseKind {
    Statement {
        expectation: StatementExpectation,
    },
    Query {
        sort_mode: SortMode,
    },
}

#[derive(Debug)]
struct Case {
    id: String,
    sql: String,
    kind: CaseKind,
}

#[derive(Default)]
struct Summary {
    pass: usize,
    known_divergence: usize,
    bug: usize,
}

#[derive(Default)]
struct Cli {
    suites: Vec<PathBuf>,
    allowlist: Option<PathBuf>,
    pg_url: Option<String>,
    pg_schema: Option<String>,
    dry_run: bool,
    keep_schema: bool,
}

fn print_usage() {
    eprintln!(
        "Usage: cargo run --bin sqlite_compat_harness -- \\
  --suite <path.slt|dir> [--suite ...] \\
  [--allowlist <known_divergences.txt>] \\
  [--pg-url <postgres://...>] \\
  [--pg-schema <schema>] \\
  [--dry-run] [--keep-schema]"
    );
}

fn parse_cli() -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut args = env::args().skip(1).peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--suite" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--suite requires a value".to_string())?;
                cli.suites.push(PathBuf::from(v));
            }
            "--allowlist" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--allowlist requires a value".to_string())?;
                cli.allowlist = Some(PathBuf::from(v));
            }
            "--pg-url" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--pg-url requires a value".to_string())?;
                cli.pg_url = Some(v);
            }
            "--pg-schema" => {
                let v = args
                    .next()
                    .ok_or_else(|| "--pg-schema requires a value".to_string())?;
                cli.pg_schema = Some(v);
            }
            "--dry-run" => cli.dry_run = true,
            "--keep-schema" => cli.keep_schema = true,
            "--help" | "-h" => {
                print_usage();
                process::exit(0);
            }
            other => {
                return Err(format!("unknown argument: {}", other));
            }
        }
    }

    if cli.suites.is_empty() {
        let default = PathBuf::from("tests/sqllogictest");
        if default.exists() {
            cli.suites.push(default);
        } else {
            return Err("at least one --suite must be provided".to_string());
        }
    }

    Ok(cli)
}

fn collect_suite_files(inputs: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for p in inputs {
        if p.is_file() {
            out.push(p.clone());
            continue;
        }
        if p.is_dir() {
            let entries = fs::read_dir(p).map_err(|e| format!("read_dir {}: {}", p.display(), e))?;
            for e in entries {
                let e = e.map_err(|err| format!("read_dir entry error: {}", err))?;
                let ep = e.path();
                if ep.extension().and_then(|s| s.to_str()) == Some("slt") {
                    out.push(ep);
                }
            }
            continue;
        }
        return Err(format!("suite path not found: {}", p.display()));
    }
    out.sort();
    out.dedup();
    if out.is_empty() {
        return Err("no .slt files found".to_string());
    }
    Ok(out)
}

fn parse_sort_mode(header: &str) -> SortMode {
    let lower = header.to_ascii_lowercase();
    if lower.contains("rowsort") {
        SortMode::RowSort
    } else if lower.contains("valuesort") {
        SortMode::ValueSort
    } else {
        SortMode::NoSort
    }
}

fn parse_cases(path: &Path) -> Result<Vec<Case>, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0usize;
    let mut cases = Vec::new();

    while i < lines.len() {
        let raw = lines[i];
        let line = raw.trim();
        let line_no = i + 1;

        if line.is_empty() || line.starts_with('#') {
            i += 1;
            continue;
        }
        if line.starts_with("onlyif ") || line.starts_with("skipif ") {
            i += 1;
            continue;
        }

        if line.starts_with("statement ") {
            let expectation = if line.to_ascii_lowercase().contains("error") {
                StatementExpectation::Error
            } else {
                StatementExpectation::Ok
            };
            i += 1;
            let mut sql_lines = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                if l.trim().is_empty() {
                    break;
                }
                sql_lines.push(l);
                i += 1;
            }
            let sql = sql_lines.join("\n").trim().to_string();
            if !sql.is_empty() {
                cases.push(Case {
                    id: format!("{}:{}", path.display(), line_no),
                    sql,
                    kind: CaseKind::Statement { expectation },
                });
            }
            while i < lines.len() && lines[i].trim().is_empty() {
                i += 1;
            }
            continue;
        }

        if line.starts_with("query ") {
            let sort_mode = parse_sort_mode(line);
            i += 1;
            let mut sql_lines = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                let t = l.trim();
                if t == "----" || t.is_empty() {
                    break;
                }
                sql_lines.push(l);
                i += 1;
            }
            if i < lines.len() && lines[i].trim() == "----" {
                i += 1;
                while i < lines.len() && !lines[i].trim().is_empty() {
                    i += 1;
                }
            }
            while i < lines.len() && lines[i].trim().is_empty() {
                i += 1;
            }
            let sql = sql_lines.join("\n").trim().to_string();
            if !sql.is_empty() {
                cases.push(Case {
                    id: format!("{}:{}", path.display(), line_no),
                    sql,
                    kind: CaseKind::Query { sort_mode },
                });
            }
            continue;
        }

        i += 1;
    }

    Ok(cases)
}

fn canonicalize_scalar(raw: &str) -> String {
    let s = raw.trim();
    if s.eq_ignore_ascii_case("null") {
        return "NULL".to_string();
    }
    if s.eq_ignore_ascii_case("t") || s.eq_ignore_ascii_case("true") {
        return "1".to_string();
    }
    if s.eq_ignore_ascii_case("f") || s.eq_ignore_ascii_case("false") {
        return "0".to_string();
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        return v.to_string();
    }
    s.to_string()
}

fn canonicalize_rows(rows: Vec<Vec<String>>, sort: SortMode) -> Vec<String> {
    let unit = '\u{001f}';
    match sort {
        SortMode::NoSort => rows
            .into_iter()
            .map(|r| r.join(&unit.to_string()))
            .collect(),
        SortMode::RowSort => {
            let mut out: Vec<String> = rows
                .into_iter()
                .map(|r| r.join(&unit.to_string()))
                .collect();
            out.sort();
            out
        }
        SortMode::ValueSort => {
            let mut vals: Vec<String> = rows.into_iter().flatten().collect();
            vals.sort();
            vec![vals.join(&unit.to_string())]
        }
    }
}

fn sqlite_query_rows(conn: &Connection, sql: &str) -> Result<Vec<Vec<String>>, String> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| format!("sqlite prepare failed: {}", e))?;
    let col_count = stmt.column_count();
    let mut rows_out = Vec::new();
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("sqlite query failed: {}", e))?;
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("sqlite row fetch failed: {}", e))?
    {
        let mut cur = Vec::with_capacity(col_count);
        for i in 0..col_count {
            let v = row
                .get_ref(i)
                .map_err(|e| format!("sqlite value read failed: {}", e))?;
            let s = match v {
                ValueRef::Null => "NULL".to_string(),
                ValueRef::Integer(n) => n.to_string(),
                ValueRef::Real(f) => {
                    if f.fract() == 0.0 {
                        format!("{:.0}", f)
                    } else {
                        f.to_string()
                    }
                }
                ValueRef::Text(t) => canonicalize_scalar(&String::from_utf8_lossy(t)),
                ValueRef::Blob(b) => format!("\\x{}", hex_lower(b)),
            };
            cur.push(canonicalize_scalar(&s));
        }
        rows_out.push(cur);
    }
    Ok(rows_out)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{:02x}", b);
    }
    out
}

fn pg_statement_exec(pg: &mut Client, schema: &str, sql: &str) -> Result<(), String> {
    let wrapped = format!("SET search_path TO {}, public; {}", schema, sql);
    pg.batch_execute(&wrapped)
        .map_err(|e| format!("pg execute failed: {}", e))
}

fn pg_query_rows(pg: &mut Client, schema: &str, sql: &str) -> Result<Vec<Vec<String>>, String> {
    let wrapped = format!("SET search_path TO {}, public; {}", schema, sql);
    let msgs = pg
        .simple_query(&wrapped)
        .map_err(|e| format!("pg query failed: {}", e))?;
    let mut out = Vec::new();
    for msg in msgs {
        if let SimpleQueryMessage::Row(r) = msg {
            let mut cur = Vec::with_capacity(r.len());
            for i in 0..r.len() {
                let val = r.get(i).unwrap_or("NULL");
                cur.push(canonicalize_scalar(val));
            }
            out.push(cur);
        }
    }
    Ok(out)
}

fn is_known_divergence(patterns: &[Regex], case: &Case, detail: &str) -> bool {
    let hay = format!("{}\n{}\n{}", case.id, case.sql, detail);
    patterns.iter().any(|re| re.is_match(&hay))
}

fn load_allowlist(path: Option<&Path>) -> Result<Vec<Regex>, String> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)
        .map_err(|e| format!("failed to read allowlist {}: {}", path.display(), e))?;
    let mut out = Vec::new();
    for (idx, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let re = Regex::new(line)
            .map_err(|e| format!("invalid regex in {}:{}: {}", path.display(), idx + 1, e))?;
        out.push(re);
    }
    Ok(out)
}

fn run_case(
    case: &Case,
    sqlite: &Connection,
    pg: &mut Client,
    schema: &str,
    allowlist: &[Regex],
    summary: &mut Summary,
) {
    let translated = match translate(&case.sql) {
        Ok(t) => t.sql,
        Err(e) => {
            let detail = format!("translate failed: {}", e);
            if is_known_divergence(allowlist, case, &detail) {
                summary.known_divergence += 1;
                eprintln!("KNOWN: {} ({})", case.id, detail);
            } else {
                summary.bug += 1;
                eprintln!("BUG: {} ({})", case.id, detail);
            }
            return;
        }
    };

    match case.kind {
        CaseKind::Statement { expectation } => {
            let sqlite_res = sqlite.execute_batch(&case.sql).map(|_| ());
            let pg_res = pg_statement_exec(pg, schema, &translated);
            let sqlite_ok = sqlite_res.is_ok();
            let pg_ok = pg_res.is_ok();
            let expect_ok = expectation == StatementExpectation::Ok;
            let pass = if expect_ok {
                sqlite_ok && pg_ok
            } else {
                !sqlite_ok && !pg_ok
            };
            if pass {
                summary.pass += 1;
                return;
            }
            let detail = format!(
                "statement mismatch expect={:?} sqlite={:?} pg={:?} translated={}",
                expectation,
                sqlite_res.err(),
                pg_res.err(),
                translated
            );
            if is_known_divergence(allowlist, case, &detail) {
                summary.known_divergence += 1;
                eprintln!("KNOWN: {} ({})", case.id, detail);
            } else {
                summary.bug += 1;
                eprintln!("BUG: {} ({})", case.id, detail);
            }
        }
        CaseKind::Query { sort_mode } => {
            let sqlite_rows = match sqlite_query_rows(sqlite, &case.sql) {
                Ok(v) => v,
                Err(e) => {
                    let detail = format!("sqlite query failed: {}", e);
                    if is_known_divergence(allowlist, case, &detail) {
                        summary.known_divergence += 1;
                        eprintln!("KNOWN: {} ({})", case.id, detail);
                    } else {
                        summary.bug += 1;
                        eprintln!("BUG: {} ({})", case.id, detail);
                    }
                    return;
                }
            };
            let pg_rows = match pg_query_rows(pg, schema, &translated) {
                Ok(v) => v,
                Err(e) => {
                    let detail = format!("pg query failed: {} translated={}", e, translated);
                    if is_known_divergence(allowlist, case, &detail) {
                        summary.known_divergence += 1;
                        eprintln!("KNOWN: {} ({})", case.id, detail);
                    } else {
                        summary.bug += 1;
                        eprintln!("BUG: {} ({})", case.id, detail);
                    }
                    return;
                }
            };
            let left = canonicalize_rows(sqlite_rows, sort_mode);
            let right = canonicalize_rows(pg_rows, sort_mode);
            if left == right {
                summary.pass += 1;
                return;
            }
            let detail = format!(
                "query result mismatch\nsqlite={:?}\npg={:?}\ntranslated={}",
                left, right, translated
            );
            if is_known_divergence(allowlist, case, &detail) {
                summary.known_divergence += 1;
                eprintln!("KNOWN: {} (result mismatch)", case.id);
            } else {
                summary.bug += 1;
                eprintln!("BUG: {} (result mismatch)", case.id);
                eprintln!("{}", detail);
            }
        }
    }
}

fn build_pg_url(cli: &Cli) -> String {
    if let Some(v) = &cli.pg_url {
        return v.clone();
    }
    if let Ok(v) = env::var("SLT_PG_URL") {
        return v;
    }
    let host = env::var("PLEX_PG_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = env::var("PLEX_PG_PORT").unwrap_or_else(|_| "5432".to_string());
    let db = env::var("PLEX_PG_DATABASE").unwrap_or_else(|_| "plex".to_string());
    let user = env::var("PLEX_PG_USER").unwrap_or_else(|_| "plex".to_string());
    let pass = env::var("PLEX_PG_PASSWORD").unwrap_or_else(|_| "plex".to_string());
    format!(
        "host={} port={} dbname={} user={} password={}",
        host, port, db, user, pass
    )
}

fn build_pg_schema(cli: &Cli) -> String {
    if let Some(v) = &cli.pg_schema {
        return v.clone();
    }
    if let Ok(v) = env::var("SLT_PG_SCHEMA") {
        return v;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("slt_{}_{}", process::id(), now)
}

fn ensure_pg_compat_functions(pg: &mut Client) -> Result<(), String> {
    pg.batch_execute(
        r#"
CREATE OR REPLACE FUNCTION public.json_valid(input text)
RETURNS boolean
LANGUAGE plpgsql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $fn$
BEGIN
    PERFORM input::jsonb;
    RETURN true;
EXCEPTION
    WHEN others THEN
        RETURN false;
END;
$fn$;

CREATE OR REPLACE FUNCTION public.jsonb_mergepatch(target jsonb, patch jsonb)
RETURNS jsonb
LANGUAGE plpgsql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $fn$
DECLARE
    result jsonb;
    k text;
    v jsonb;
BEGIN
    IF jsonb_typeof(patch) <> 'object' THEN
        RETURN patch;
    END IF;

    IF jsonb_typeof(target) <> 'object' THEN
        result := '{}'::jsonb;
    ELSE
        result := target;
    END IF;

    FOR k, v IN SELECT e.key, e.value FROM jsonb_each(patch) AS e(key, value) LOOP
        IF v = 'null'::jsonb THEN
            result := result - k;
        ELSIF (result ? k)
              AND jsonb_typeof(result -> k) = 'object'
              AND jsonb_typeof(v) = 'object' THEN
            result := jsonb_set(result, ARRAY[k], public.jsonb_mergepatch(result -> k, v), true);
        ELSE
            result := jsonb_set(result, ARRAY[k], v, true);
        END IF;
    END LOOP;

    RETURN result;
END;
$fn$;
"#,
    )
    .map_err(|e| format!("failed to install pg compat functions: {}", e))
}

fn main() {
    let cli = match parse_cli() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {}", e);
            print_usage();
            process::exit(2);
        }
    };

    let suite_files = match collect_suite_files(&cli.suites) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {}", e);
            process::exit(2);
        }
    };
    let allowlist = match load_allowlist(cli.allowlist.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {}", e);
            process::exit(2);
        }
    };

    let mut parsed = Vec::new();
    for p in &suite_files {
        match parse_cases(p) {
            Ok(mut c) => parsed.append(&mut c),
            Err(e) => {
                eprintln!("error: {}", e);
                process::exit(2);
            }
        }
    }

    println!(
        "Loaded {} cases from {} suite files.",
        parsed.len(),
        suite_files.len()
    );

    if cli.dry_run {
        for c in &parsed {
            println!("DRY {}", c.id);
        }
        process::exit(0);
    }

    let pg_url = build_pg_url(&cli);
    let schema = build_pg_schema(&cli);
    let mut pg = match Client::connect(&pg_url, NoTls) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to connect postgres: {}", e);
            process::exit(2);
        }
    };
    if let Err(e) = pg.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {}", schema)) {
        eprintln!("error: failed creating schema {}: {}", schema, e);
        process::exit(2);
    }
    if let Err(e) = ensure_pg_compat_functions(&mut pg) {
        eprintln!("error: {}", e);
        process::exit(2);
    }

    let sqlite = match Connection::open_in_memory() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to create sqlite in-memory db: {}", e);
            process::exit(2);
        }
    };

    let mut summary = Summary::default();
    for case in &parsed {
        run_case(case, &sqlite, &mut pg, &schema, &allowlist, &mut summary);
    }

    if !cli.keep_schema {
        let _ = pg.batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", schema));
    } else {
        println!("Keeping schema {}", schema);
    }

    println!(
        "Summary: pass={} known_divergence={} bug={}",
        summary.pass, summary.known_divergence, summary.bug
    );
    if summary.bug > 0 {
        process::exit(1);
    }
}
