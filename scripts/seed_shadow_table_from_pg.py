#!/usr/bin/env python3
"""
Seed a SQLite shadow table from PostgreSQL using COPY text output.

This is intentionally non-destructive:
- rows are inserted with INSERT OR IGNORE
- existing SQLite rows are preserved

Usage:
    seed_shadow_table_from_pg.py <sqlite_db> <table> <schema>
"""

import io
import os
import re
import sqlite3
import subprocess
import sys
from typing import Dict, Iterable, List, Sequence


IDENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
INT_TYPES = {"smallint", "integer", "bigint"}
FLOAT_TYPES = {"real", "double precision"}
TEXT_TYPES = {
    "text",
    "character varying",
    "character",
    "varchar",
    "char",
    "json",
    "jsonb",
    "uuid",
    "date",
    "timestamp without time zone",
    "timestamp with time zone",
    "time without time zone",
    "time with time zone",
}


def fail(msg: str) -> "NoReturn":
    print(msg, file=sys.stderr)
    sys.exit(1)


def validate_ident(value: str, label: str) -> str:
    if not IDENT_RE.fullmatch(value):
        fail(f"invalid {label}: {value!r}")
    return value


def qident(value: str) -> str:
    return '"' + value.replace('"', '""') + '"'


def run_psql_lines(sql: str) -> List[str]:
    proc = subprocess.run(
        ["psql", "-X", "-q", "-t", "-A", "-v", "ON_ERROR_STOP=1", "-c", sql],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=os.environ,
    )
    if proc.returncode != 0:
        err = proc.stderr.strip() or "psql query failed"
        fail(err)
    return [line for line in proc.stdout.splitlines() if line]


def sqlite_columns(conn: sqlite3.Connection, table: str) -> List[str]:
    table_sql = table.replace('"', '""')
    rows = conn.execute(f'PRAGMA table_info("{table_sql}")').fetchall()
    if not rows:
        fail(f"sqlite table not found: {table}")
    return [row[1] for row in rows]


def pg_columns(schema: str, table: str) -> Dict[str, str]:
    sql = (
        "SELECT column_name || E'\\t' || data_type "
        "FROM information_schema.columns "
        f"WHERE table_schema = '{schema}' AND table_name = '{table}' "
        "ORDER BY ordinal_position"
    )
    result: Dict[str, str] = {}
    for line in run_psql_lines(sql):
        col, data_type = line.split("\t", 1)
        result[col] = data_type
    if not result:
        fail(f"postgres table not found or has no columns: {schema}.{table}")
    return result


def decode_copy_field(raw: str) -> str:
    if "\\" not in raw:
        return raw

    out: List[str] = []
    i = 0
    n = len(raw)
    while i < n:
        ch = raw[i]
        if ch != "\\" or i + 1 >= n:
            out.append(ch)
            i += 1
            continue

        nxt = raw[i + 1]
        if nxt == "b":
            out.append("\b")
            i += 2
            continue
        if nxt == "f":
            out.append("\f")
            i += 2
            continue
        if nxt == "n":
            out.append("\n")
            i += 2
            continue
        if nxt == "r":
            out.append("\r")
            i += 2
            continue
        if nxt == "t":
            out.append("\t")
            i += 2
            continue
        if nxt == "v":
            out.append("\v")
            i += 2
            continue
        if nxt == "\\":
            out.append("\\")
            i += 2
            continue
        if nxt == "x":
            hex_digits = []
            j = i + 2
            while j < n and len(hex_digits) < 2 and raw[j] in "0123456789abcdefABCDEF":
                hex_digits.append(raw[j])
                j += 1
            if hex_digits:
                out.append(chr(int("".join(hex_digits), 16)))
                i = j
                continue
        if nxt in "01234567":
            oct_digits = [nxt]
            j = i + 2
            while j < n and len(oct_digits) < 3 and raw[j] in "01234567":
                oct_digits.append(raw[j])
                j += 1
            out.append(chr(int("".join(oct_digits), 8)))
            i = j
            continue

        out.append(nxt)
        i += 2

    return "".join(out)


def convert_value(raw: str, data_type: str):
    if raw == r"\N":
        return None

    decoded = decode_copy_field(raw)

    if data_type == "bytea":
        return bytes.fromhex(decoded) if decoded else b""
    if data_type in INT_TYPES:
        return int(decoded)
    if data_type in FLOAT_TYPES:
        return float(decoded)
    if data_type == "boolean":
        return 1 if decoded in {"t", "true", "1"} else 0
    if data_type in TEXT_TYPES:
        return decoded
    return decoded


def build_select_list(columns: Sequence[str], pg_types: Dict[str, str]) -> str:
    parts = []
    for column in columns:
        quoted = qident(column)
        if pg_types[column] == "bytea":
            parts.append(f"encode({quoted}, 'hex') AS {quoted}")
        else:
            parts.append(quoted)
    return ", ".join(parts)


def stream_pg_rows(schema: str, table: str, columns: Sequence[str], pg_types: Dict[str, str]):
    select_list = build_select_list(columns, pg_types)
    copy_sql = (
        f"COPY (SELECT {select_list} FROM {qident(schema)}.{qident(table)}) "
        "TO STDOUT WITH (FORMAT text, DELIMITER E'\\t', NULL '\\N')"
    )

    proc = subprocess.Popen(
        ["psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-c", copy_sql],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=os.environ,
    )

    if proc.stdout is None or proc.stderr is None:
        fail("failed to start psql COPY")

    stderr_chunks: List[bytes] = []
    try:
        stream = io.TextIOWrapper(proc.stdout, encoding="utf-8", newline="")
        for line in stream:
            line = line.rstrip("\n")
            if line.endswith("\r"):
                line = line[:-1]
            fields = line.split("\t")
            if len(fields) != len(columns):
                fail(
                    f"unexpected field count for {schema}.{table}: "
                    f"expected {len(columns)}, got {len(fields)}"
                )
            yield [
                convert_value(field, pg_types[column])
                for column, field in zip(columns, fields)
            ]
    finally:
        stderr_chunks.append(proc.stderr.read())
        rc = proc.wait()
        if rc != 0:
            err = b"".join(stderr_chunks).decode("utf-8", errors="replace").strip()
            fail(err or f"psql COPY failed for {schema}.{table}")


def batched(rows: Iterable[Sequence[object]], size: int) -> Iterable[List[Sequence[object]]]:
    batch: List[Sequence[object]] = []
    for row in rows:
        batch.append(row)
        if len(batch) >= size:
            yield batch
            batch = []
    if batch:
        yield batch


def main() -> None:
    if len(sys.argv) != 4:
        fail(f"usage: {sys.argv[0]} <sqlite_db> <table> <schema>")

    sqlite_db = sys.argv[1]
    table = validate_ident(sys.argv[2], "table")
    schema = validate_ident(sys.argv[3], "schema")

    conn = sqlite3.connect(sqlite_db)
    try:
        sqlite_cols = sqlite_columns(conn, table)
        pg_types = pg_columns(schema, table)
        common_cols = [col for col in sqlite_cols if col in pg_types]
        if not common_cols:
            fail(f"no common columns between sqlite and postgres for {schema}.{table}")

        placeholders = ", ".join(["?"] * len(common_cols))
        quoted_cols = ", ".join(qident(col) for col in common_cols)
        sql = f"INSERT OR IGNORE INTO {qident(table)} ({quoted_cols}) VALUES ({placeholders})"

        before = conn.total_changes
        attempted = 0
        with conn:
            for batch in batched(stream_pg_rows(schema, table, common_cols, pg_types), 500):
                attempted += len(batch)
                conn.executemany(sql, batch)
        inserted = conn.total_changes - before
        print(
            f"seeded {table} into {os.path.basename(sqlite_db)}: "
            f"attempted={attempted} inserted={inserted}"
        )
    finally:
        conn.close()


if __name__ == "__main__":
    main()
