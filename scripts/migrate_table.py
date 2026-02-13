#!/usr/bin/env python3
"""
Migrate a single table from SQLite to PostgreSQL using COPY protocol.

Avoids the CSV truncation bug in sqlite3 CLI where large TEXT fields
(>8KB with embedded quotes) get silently truncated during CSV export.

Usage: migrate_table.py <sqlite_db> <table> <select_expr> <pg_cols> <schema>
"""

import os
import sys
import sqlite3
import subprocess
import io


def main():
    if len(sys.argv) != 6:
        print(f"Usage: {sys.argv[0]} <sqlite_db> <table> <select_expr> <pg_cols> <schema>",
              file=sys.stderr)
        sys.exit(1)

    sqlite_db = sys.argv[1]
    table = sys.argv[2]
    select_expr = sys.argv[3]
    pg_cols = sys.argv[4]
    schema = sys.argv[5]

    # Connect to SQLite
    conn = sqlite3.connect(sqlite_db)
    conn.text_factory = str
    cur = conn.cursor()

    # Read all rows
    sql = f'SELECT {select_expr} FROM "{table}"'
    cur.execute(sql)
    rows = cur.fetchall()
    conn.close()

    if not rows:
        sys.exit(0)

    # Build tab-separated data for COPY
    # COPY uses \t as delimiter, \n as newline, \N for NULL
    # Backslashes in data must be escaped as \\
    buf = io.StringIO()
    for row in rows:
        fields = []
        for val in row:
            if val is None:
                fields.append("\\N")
            elif isinstance(val, bytes):
                # BLOB: encode as PostgreSQL bytea hex format
                fields.append("\\\\x" + val.hex())
            elif isinstance(val, str):
                # Escape backslashes, tabs, and newlines for COPY format
                escaped = val.replace("\\", "\\\\")
                escaped = escaped.replace("\t", "\\t")
                escaped = escaped.replace("\n", "\\n")
                escaped = escaped.replace("\r", "\\r")
                fields.append(escaped)
            else:
                fields.append(str(val))
        buf.write("\t".join(fields) + "\n")

    copy_data = buf.getvalue()
    buf.close()

    # Stream to PostgreSQL via psql COPY FROM STDIN
    copy_cmd = f"COPY {schema}.\"{table}\"({pg_cols}) FROM STDIN"

    env = os.environ.copy()
    proc = subprocess.Popen(
        ["psql", "-q", "-c", copy_cmd],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    stdout, stderr = proc.communicate(input=copy_data.encode("utf-8"))

    if proc.returncode != 0:
        print(f"COPY failed for {table}: {stderr.decode()}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
