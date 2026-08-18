"""Refresh stale case snapshots in the ARM Bench database.

The web service used to snapshot .sy/.in/.out files with Path.read_text(),
whose universal-newline translation silently stripped CRLF. The Docker
harness reads the same files with read_bytes(). Any case whose files contain
CRLF therefore got a corrupted snapshot; functional/68_brainfk (CRLF in its
.out) reported WA for both clang and SOYO although both programs were correct.

Run this after deploying the read-case-file fix so existing task_cases point
at byte-exact blobs again, and in-place retries stop reusing the damaged
snapshots:

    uv run python -m arm_bench.resync_cases --dry-run
    uv run python -m arm_bench.resync_cases

Idempotent: only rows whose stored content differs from the file by exactly
the universal-newline translation are repaired; rows whose content differs for
any other reason (file edited since the task ran) are left untouched.
"""

from __future__ import annotations

import argparse
import hashlib
import sqlite3
from pathlib import Path
from typing import Any

from arm_bench import server
from arm_bench.server import connect_db, delete_unused_blobs

FILE_FOR_COLUMN = {
    'source_blob_hash': lambda path: path,
    'input_blob_hash': lambda path: path.with_suffix('.in'),
    'expected_blob_hash': lambda path: path.with_suffix('.out'),
}


def _universal_newlines(data: bytes) -> bytes:
    """Reproduce Path.read_text()'s newline translation on bytes."""
    return data.replace(b'\r\n', b'\n').replace(b'\r', b'\n')


def _store(connection: sqlite3.Connection, raw: bytes) -> str:
    digest = hashlib.sha256(raw).hexdigest()
    connection.execute(
        'INSERT OR IGNORE INTO content_blobs (hash, data) VALUES (?, ?)', (digest, raw)
    )
    return digest


def resync_case_snapshots(
    database_path: Path, tests_root: Path | None = None, dry_run: bool = False
) -> dict[str, Any]:
    """Repair CRLF-corrupted source/input/expected snapshots in task_cases.

    Returns a report of what was checked and changed; with dry_run=True the
    database is left untouched.
    """
    tests_root = Path(tests_root) if tests_root is not None else server.TESTS
    checked = rows_updated = expected_output_fixed = 0
    columns_updated = {column: 0 for column in FILE_FOR_COLUMN}
    changes: list[dict[str, Any]] = []
    with connect_db(database_path) as connection:
        rows = connection.execute(
            'SELECT id, case_id, source_path, source_blob_hash, input_blob_hash, '
            'expected_blob_hash, expected_output_blob_hash FROM task_cases'
        ).fetchall()
        for row in rows:
            source_path = Path(row['source_path'])
            if not source_path.is_file():
                candidate = tests_root / row['case_id']
                source_path = candidate if candidate.is_file() else source_path
            row_changes: list[str] = []
            for column, resolve in FILE_FOR_COLUMN.items():
                file_path = resolve(source_path)
                stored_hash = row[column]
                if stored_hash is None or not file_path.is_file():
                    continue
                raw = file_path.read_bytes()
                blob = connection.execute(
                    'SELECT data FROM content_blobs WHERE hash = ?', (stored_hash,)
                ).fetchone()
                if blob is None or blob['data'] == raw:
                    continue
                if blob['data'] != _universal_newlines(raw):
                    continue  # legitimately different snapshot; leave it alone
                correct_hash = _store(connection, raw)
                if not dry_run:
                    connection.execute(
                        f'UPDATE task_cases SET {column} = ? WHERE id = ?',
                        (correct_hash, row['id']),
                    )
                if (
                    column == 'expected_blob_hash'
                    and row['expected_output_blob_hash'] == stored_hash
                ):
                    if not dry_run:
                        connection.execute(
                            'UPDATE task_cases SET expected_output_blob_hash = ? WHERE id = ?',
                            (correct_hash, row['id']),
                        )
                    expected_output_fixed += 1
                columns_updated[column] += 1
                row_changes.append(column)
            checked += 1
            if row_changes:
                rows_updated += 1
                changes.append(
                    {'task_case_id': row['id'], 'case_id': row['case_id'], 'columns': row_changes}
                )
        if not dry_run:
            delete_unused_blobs(connection)
        connection.execute('PRAGMA optimize')
    return {
        'checked': checked,
        'rows_updated': rows_updated,
        'columns_updated': columns_updated,
        'expected_output_fixed': expected_output_fixed,
        'changes': changes,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        '--database', default=str(server.DB_PATH), help='path to the arm-bench sqlite database'
    )
    parser.add_argument('--tests', default=str(server.TESTS), help='test cases root directory')
    parser.add_argument('--dry-run', action='store_true', help='report only, change nothing')
    args = parser.parse_args()
    result = resync_case_snapshots(Path(args.database), Path(args.tests), dry_run=args.dry_run)
    verb = 'would update' if args.dry_run else 'updated'
    print(f'{verb} {result["rows_updated"]} of {result["checked"]} task cases')
    print(f'columns: {result["columns_updated"]}')
    print(f'expected_output hashes refreshed: {result["expected_output_fixed"]}')
    for change in result['changes']:
        print(f'  {change["case_id"]}: {", ".join(change["columns"])}')


if __name__ == '__main__':
    main()
