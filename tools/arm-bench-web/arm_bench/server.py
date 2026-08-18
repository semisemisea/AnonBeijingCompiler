from __future__ import annotations

import asyncio
import hashlib
import json
import re
import shutil
import sqlite3
import statistics
import subprocess
import time
import uuid
from collections import Counter
from contextlib import asynccontextmanager, suppress
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Literal

import httpx2
from fastapi import FastAPI, HTTPException, Request, WebSocket
from fastapi.responses import FileResponse, Response
from pydantic import BaseModel, Field

from arm_bench.board import BoardClient, BoardConfig, CaseRun, prepare_toolchain

HERE = Path(__file__).resolve().parents[1]
ROOT = HERE.parents[1]
TESTS = ROOT / 'tests'
DATA = HERE / 'data'
DB_PATH = DATA / 'arm-bench.sqlite3'
CASE_SUITES = ('functional', 'h_functional', 'perf')
CASE_NAME_PATTERN = re.compile(r'[A-Za-z0-9][A-Za-z0-9_.-]*')

TERMINAL_TASK_STATUSES = {'COMPLETE', 'CANCELLED', 'ERROR'}
TERMINAL_CASE_STATUSES = {'PASS', 'WA', 'CE', 'RE', 'TLE', 'ERROR', 'CANCELLED'}
RETRYABLE_CASE_STATUSES = {'WA', 'CE', 'RE', 'TLE', 'ERROR', 'CANCELLED'}
CASE_CONTENT_FIELDS = {
    'input': ('input_blob_hash', 'input.in'),
    'compile_stdout': ('compile_stdout_blob_hash', 'compile.stdout.txt'),
    'compile_stderr': ('compile_stderr_blob_hash', 'compile.stderr.txt'),
    'stdout': ('stdout_blob_hash', 'stdout.txt'),
    'stderr': ('stderr_blob_hash', 'stderr.txt'),
    'expected_output': ('expected_output_blob_hash', 'expected.out'),
    'actual_output': ('actual_output_blob_hash', 'actual.out'),
    'error': ('error_blob_hash', 'error.txt'),
}
BLOB_HASH_COLUMNS = (
    'source_blob_hash',
    'input_blob_hash',
    'expected_blob_hash',
    'compile_stdout_blob_hash',
    'compile_stderr_blob_hash',
    'stdout_blob_hash',
    'stderr_blob_hash',
    'expected_output_blob_hash',
    'actual_output_blob_hash',
    'error_blob_hash',
)
CASE_SUMMARY_COLUMNS = """
  id, task_id, position, case_id, suite, name, status,
  started_at, run_started_at, finished_at, returncode, median_ms, min_ms, max_ms
"""
CASE_DETAIL_COLUMNS = """
  id, task_id, position, case_id, suite, name, source_path, status,
  started_at, run_started_at, finished_at, compile_command, run_command, remote_path,
  returncode, warmup_samples_json, samples_json, median_ms, min_ms, max_ms,
  artifact_dir,
  (SELECT length(data) FROM content_blobs WHERE hash = input_blob_hash) AS input_size,
  (SELECT length(data) FROM content_blobs WHERE hash = compile_stdout_blob_hash)
    AS compile_stdout_size,
  (SELECT length(data) FROM content_blobs WHERE hash = compile_stderr_blob_hash)
    AS compile_stderr_size,
  (SELECT length(data) FROM content_blobs WHERE hash = stdout_blob_hash) AS stdout_size,
  (SELECT length(data) FROM content_blobs WHERE hash = stderr_blob_hash) AS stderr_size,
  (SELECT length(data) FROM content_blobs WHERE hash = expected_output_blob_hash)
    AS expected_output_size,
  (SELECT length(data) FROM content_blobs WHERE hash = actual_output_blob_hash)
    AS actual_output_size,
  (SELECT length(data) FROM content_blobs WHERE hash = error_blob_hash) AS error_size
"""


@dataclass(frozen=True)
class Case:
    id: str
    suite: str
    name: str
    has_input: bool
    has_expected: bool


class BoardUpdate(BaseModel):
    host: str = '192.168.77.2'
    port: int = 8766
    cpu: int = Field(default=2, ge=0)


class TaskCreate(BaseModel):
    compiler: Literal['SOYO', 'CLANG']
    opt_level: int = Field(default=2, ge=0, le=3)
    cases: list[str]
    warmups: int = Field(default=0, ge=0)
    repeats: int = Field(default=1, ge=1)
    timeout_seconds: int = Field(default=180, ge=1)


class CaseWrite(BaseModel):
    suite: Literal['functional', 'h_functional', 'perf']
    name: str
    source: str
    input: str | None = None
    expected: str | None = None


def connect_db(path: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(path)
    connection.row_factory = sqlite3.Row
    connection.execute('PRAGMA foreign_keys = ON')
    return connection


def put_blob(connection: sqlite3.Connection, value: str | None) -> str | None:
    if value is None:
        return None
    data = value.encode()
    digest = hashlib.sha256(data).hexdigest()
    connection.execute(
        'INSERT OR IGNORE INTO content_blobs (hash, data) VALUES (?, ?)', (digest, data)
    )
    return digest


def get_blob(connection: sqlite3.Connection, digest: str | None) -> str | None:
    if digest is None:
        return None
    row = connection.execute('SELECT data FROM content_blobs WHERE hash = ?', (digest,)).fetchone()
    return bytes(row['data']).decode()


def delete_unused_blobs(connection: sqlite3.Connection) -> None:
    references = ' OR '.join(f'content_blobs.hash = {column}' for column in BLOB_HASH_COLUMNS)
    connection.execute(
        f'DELETE FROM content_blobs WHERE NOT EXISTS (SELECT 1 FROM task_cases WHERE {references})'
    )


def reset_task_cases(connection: sqlite3.Connection, case_ids: list[str]) -> None:
    placeholders = ', '.join('?' for _ in case_ids)
    connection.execute(f'DELETE FROM baselines WHERE task_case_id IN ({placeholders})', case_ids)
    connection.execute(
        f"""
        UPDATE task_cases SET status = 'QUEUED', started_at = NULL, run_started_at = NULL,
          finished_at = NULL, compile_stdout_blob_hash = NULL,
          compile_stderr_blob_hash = NULL, compile_command = NULL, run_command = NULL,
          remote_path = NULL, stdout_blob_hash = NULL, stderr_blob_hash = NULL,
          returncode = NULL, expected_output_blob_hash = expected_blob_hash,
          actual_output_blob_hash = NULL, warmup_samples_json = NULL, samples_json = NULL,
          median_ms = NULL, min_ms = NULL, max_ms = NULL, error_blob_hash = NULL,
          artifact_dir = NULL
        WHERE id IN ({placeholders})
        """,
        case_ids,
    )
    delete_unused_blobs(connection)


def init_db(path: Path = DB_PATH) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with connect_db(path) as connection:
        connection.executescript(
            """
      CREATE TABLE IF NOT EXISTS tasks (
        number INTEGER PRIMARY KEY AUTOINCREMENT,
        id TEXT NOT NULL UNIQUE,
        git_hash TEXT NOT NULL,
        dirty INTEGER NOT NULL,
        compiler TEXT NOT NULL,
        opt_level INTEGER NOT NULL,
        warmups INTEGER NOT NULL,
        repeats INTEGER NOT NULL,
        timeout_seconds INTEGER NOT NULL,
        board_host TEXT NOT NULL,
        board_port INTEGER NOT NULL,
        cpu INTEGER NOT NULL,
        status TEXT NOT NULL,
        created_at REAL NOT NULL,
        started_at REAL,
        finished_at REAL
      );

      CREATE TABLE IF NOT EXISTS task_cases (
        id TEXT PRIMARY KEY,
        task_id TEXT NOT NULL,
        position INTEGER NOT NULL,
        case_id TEXT NOT NULL,
        suite TEXT NOT NULL,
        name TEXT NOT NULL,
        source_path TEXT NOT NULL,
        source_blob_hash TEXT NOT NULL,
        input_blob_hash TEXT,
        expected_blob_hash TEXT,
        status TEXT NOT NULL,
        started_at REAL,
        run_started_at REAL,
        finished_at REAL,
        compile_stdout_blob_hash TEXT,
        compile_stderr_blob_hash TEXT,
        compile_command TEXT,
        run_command TEXT,
        remote_path TEXT,
        stdout_blob_hash TEXT,
        stderr_blob_hash TEXT,
        returncode INTEGER,
        expected_output_blob_hash TEXT,
        actual_output_blob_hash TEXT,
        warmup_samples_json TEXT,
        samples_json TEXT,
        median_ms REAL,
        min_ms REAL,
        max_ms REAL,
        error_blob_hash TEXT,
        artifact_dir TEXT,
        FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE CASCADE,
        UNIQUE(task_id, position)
      );

      CREATE INDEX IF NOT EXISTS task_cases_task_id ON task_cases(task_id);

      CREATE TABLE IF NOT EXISTS events (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        type TEXT NOT NULL,
        task_id TEXT,
        case_id TEXT,
        payload_json TEXT NOT NULL,
        created_at REAL NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_events_task_seq ON events(task_id, seq);

      CREATE TABLE IF NOT EXISTS baselines (
        case_id TEXT PRIMARY KEY,
        task_case_id TEXT NOT NULL UNIQUE,
        FOREIGN KEY(task_case_id) REFERENCES task_cases(id) ON DELETE CASCADE
      );

      CREATE TABLE IF NOT EXISTS board_config (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        host TEXT NOT NULL,
        port INTEGER NOT NULL,
        cpu INTEGER NOT NULL
      );

      INSERT OR IGNORE INTO board_config (id, host, port, cpu)
      VALUES (1, '192.168.77.2', 8766, 2);

      CREATE INDEX IF NOT EXISTS idx_task_cases_case_id ON task_cases(case_id);

      CREATE TABLE IF NOT EXISTS content_blobs (
        hash TEXT PRIMARY KEY,
        data BLOB NOT NULL
      );
      """
        )
        connection.execute('PRAGMA optimize')


def list_cases() -> list[Case]:
    cases = []
    for suite in CASE_SUITES:
        for source in sorted((TESTS / suite).glob('*.sy')):
            cases.append(
                Case(
                    id=f'{suite}/{source.name}',
                    suite=suite,
                    name=source.stem,
                    has_input=source.with_suffix('.in').exists(),
                    has_expected=source.with_suffix('.out').exists(),
                )
            )
    return cases


def target_case_path(suite: str, name: str) -> Path:
    stem = name.removesuffix('.sy')
    if suite not in CASE_SUITES or CASE_NAME_PATTERN.fullmatch(stem) is None:
        raise HTTPException(400, 'Invalid test case name')
    return TESTS / suite / f'{stem}.sy'


def read_case_file(path: Path) -> str:
    """Read a test-case file byte-exactly, without newline translation.

    Path.read_text() applies universal-newline translation and would silently
    strip CRLF, corrupting .out snapshots: the official Docker harness reads
    these files with read_bytes(), so a read_text()-based snapshot can fail a
    correct program (see functional/68_brainfk, whose .out ends with CRLF).
    Test-case files are UTF-8 text.
    """
    return path.read_bytes().decode('utf-8')


def write_case_file(path: Path, content: str) -> None:
    path.write_bytes(content.encode())


def case_contents(source: Path) -> dict[str, Any]:
    input_path = source.with_suffix('.in')
    expected_path = source.with_suffix('.out')
    return {
        'id': f'{source.parent.name}/{source.name}',
        'suite': source.parent.name,
        'name': source.stem,
        'source': read_case_file(source),
        'input': read_case_file(input_path) if input_path.exists() else None,
        'expected': read_case_file(expected_path) if expected_path.exists() else None,
    }


def write_case(source: Path, body: CaseWrite) -> None:
    source.parent.mkdir(parents=True, exist_ok=True)
    write_case_file(source, body.source)
    for path, content in (
        (source.with_suffix('.in'), body.input),
        (source.with_suffix('.out'), body.expected),
    ):
        if content is None:
            path.unlink(missing_ok=True)
        else:
            write_case_file(path, content)


def delete_case_files(source: Path) -> None:
    source.unlink(missing_ok=True)
    source.with_suffix('.in').unlink(missing_ok=True)
    source.with_suffix('.out').unlink(missing_ok=True)


def resolve_case(case_id: str) -> Path:
    candidate = (TESTS / case_id).resolve()
    if (
        TESTS.resolve() not in candidate.parents
        or candidate.suffix != '.sy'
        or not candidate.is_file()
    ):
        raise HTTPException(404, f'Unknown test case: {case_id}')
    return candidate


def git_revision() -> tuple[str, bool]:
    revision = subprocess.run(
        ['git', 'rev-parse', '--short=8', 'HEAD'],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    dirty = bool(
        subprocess.run(
            ['git', 'status', '--porcelain'],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )
    return revision, dirty


def database(request: Request) -> Path:
    return request.app.state.database_path


def public_board(config: BoardUpdate) -> dict[str, Any]:
    return {
        'host': config.host,
        'port': config.port,
        'cpu': config.cpu,
        'service': 'soyo-benchd',
    }


def case_detail_record(row: sqlite3.Row) -> dict[str, Any]:
    record = dict(row)
    artifact_dir_text = record.pop('artifact_dir')
    record['samples'] = json.loads(record.pop('samples_json')) if record['samples_json'] else []
    record['warmup_samples'] = (
        json.loads(record.pop('warmup_samples_json')) if record['warmup_samples_json'] else []
    )
    record['content_sizes'] = {kind: record.pop(f'{kind}_size') for kind in CASE_CONTENT_FIELDS}
    artifact_dir = Path(artifact_dir_text) if artifact_dir_text else None
    record['artifacts'] = [
        kind
        for kind, filename in (
            ('assembly', 'program.s'),
            ('elf', 'program.elf'),
            ('raana', 'program.raana'),
        )
        if artifact_dir and (artifact_dir / filename).is_file()
    ]
    return record


def baseline_records(path: Path) -> dict[str, dict[str, Any]]:
    with connect_db(path) as connection:
        rows = connection.execute(
            """
      SELECT b.case_id, tc.id, tc.task_id, tc.status, tc.median_ms,
             t.number AS task_number, t.git_hash, t.dirty, t.compiler
      FROM baselines b
      JOIN task_cases tc ON tc.id = b.task_case_id
      JOIN tasks t ON t.id = tc.task_id
      """
        ).fetchall()
    return {
        row['case_id']: {
            'id': row['id'],
            'task_id': row['task_id'],
            'status': row['status'],
            'median_ms': row['median_ms'],
            'task_number': row['task_number'],
            'git_hash': row['git_hash'],
            'dirty': bool(row['dirty']),
            'compiler': row['compiler'],
        }
        for row in rows
    }


def task_records(path: Path, task_id: str | None = None) -> list[dict[str, Any]]:
    with connect_db(path) as connection:
        where = 'WHERE id = ?' if task_id else ''
        parameters = (task_id,) if task_id else ()
        tasks = connection.execute(
            f"""
            SELECT * FROM tasks {where}
            ORDER BY CASE status WHEN 'RUNNING' THEN 0 WHEN 'QUEUED' THEN 1 ELSE 2 END,
                     CASE WHEN status IN ('RUNNING', 'QUEUED') THEN number END,
                     CASE WHEN status NOT IN ('RUNNING', 'QUEUED') THEN created_at END DESC
            """,
            parameters,
        ).fetchall()
        if task_id:
            case_rows = connection.execute(
                f'SELECT {CASE_SUMMARY_COLUMNS} FROM task_cases WHERE task_id = ? ORDER BY position',
                (task_id,),
            ).fetchall()
        else:
            case_rows = connection.execute(
                f'SELECT {CASE_SUMMARY_COLUMNS} FROM task_cases ORDER BY task_id, position'
            ).fetchall()
    baselines = baseline_records(path)
    cases_by_task: dict[str, list[dict[str, Any]]] = {task['id']: [] for task in tasks}
    for row in case_rows:
        case = dict(row)
        case['baseline'] = baselines.get(case['case_id'])
        cases_by_task[case['task_id']].append(case)
    records = []
    for task in tasks:
        cases = cases_by_task[task['id']]
        counts = Counter(case['status'] for case in cases)
        completed = sum(counts[status] for status in TERMINAL_CASE_STATUSES)
        records.append(
            {
                **dict(task),
                'dirty': bool(task['dirty']),
                'cases': cases,
                'completed_cases': completed,
                'total_cases': len(cases),
                'status_counts': dict(counts),
            }
        )
    return records


def task_record(path: Path, task_id: str) -> dict[str, Any]:
    records = task_records(path, task_id)
    if not records:
        raise HTTPException(404, 'Unknown task')
    return records[0]


def ordered_tasks(path: Path) -> list[dict[str, Any]]:
    return task_records(path)


def queue_records(path: Path) -> list[dict[str, Any]]:
    return [task for task in ordered_tasks(path) if task['status'] in {'RUNNING', 'QUEUED'}]


async def emit_event(
    app: FastAPI,
    event_type: str,
    task_id: str | None = None,
    case_id: str | None = None,
    payload: dict[str, Any] | None = None,
) -> int:
    created_at = time.time()
    with connect_db(app.state.database_path) as connection:
        cursor = connection.execute(
            'INSERT INTO events (type, task_id, case_id, payload_json, created_at) VALUES (?, ?, ?, ?, ?)',
            (event_type, task_id, case_id, json.dumps(payload or {}), created_at),
        )
        seq = cursor.lastrowid
    async with app.state.event_condition:
        app.state.event_condition.notify_all()
    return int(seq)


def event_records(path: Path, after: int, task_id: str | None = None) -> list[dict[str, Any]]:
    with connect_db(path) as connection:
        if task_id is None:
            rows = connection.execute(
                'SELECT * FROM events WHERE seq > ? ORDER BY seq', (after,)
            ).fetchall()
        else:
            rows = connection.execute(
                'SELECT * FROM events WHERE task_id = ? AND seq > ? ORDER BY seq',
                (task_id, after),
            ).fetchall()
    return [
        {
            'seq': row['seq'],
            'type': row['type'],
            'taskId': row['task_id'],
            'caseId': row['case_id'],
            'payload': json.loads(row['payload_json']),
            'at': row['created_at'],
        }
        for row in rows
    ]


def latest_seq(path: Path) -> int:
    with connect_db(path) as connection:
        return connection.execute('SELECT COALESCE(MAX(seq), 0) FROM events').fetchone()[0]


async def wait_for_event(app: FastAPI, cursor: int) -> None:
    async with app.state.event_condition:
        if latest_seq(app.state.database_path) == cursor:
            await app.state.event_condition.wait()


async def wait_for_disconnect(websocket: WebSocket) -> None:
    while (await websocket.receive())['type'] != 'websocket.disconnect':
        pass


async def run_case(app: FastAPI, board: BoardClient, task: sqlite3.Row, case: sqlite3.Row) -> None:
    now = time.time()
    with connect_db(app.state.database_path) as connection:
        source = get_blob(connection, case['source_blob_hash'])
        input_data = get_blob(connection, case['input_blob_hash'])
        expected_output = get_blob(connection, case['expected_blob_hash'])
        connection.execute(
            "UPDATE task_cases SET status = 'COMP', started_at = ?, "
            'run_started_at = NULL WHERE id = ?',
            (now, case['id']),
        )
    app.state.current_case_id = case['id']
    await emit_event(
        app,
        'case.compiling',
        task['id'],
        case['id'],
        {'case': case['case_id'], 'started_at': now},
    )

    async def mark_running() -> None:
        run_started_at = time.time()
        with connect_db(app.state.database_path) as connection:
            connection.execute(
                "UPDATE task_cases SET status = 'RUN', run_started_at = ? "
                "WHERE id = ? AND status = 'COMP'",
                (run_started_at, case['id']),
            )
        await emit_event(
            app,
            'case.running',
            task['id'],
            case['id'],
            {'case': case['case_id'], 'run_started_at': run_started_at},
        )

    artifact_dir = app.state.artifact_root / task['id'] / case['id']
    execution = asyncio.create_task(
        board.run_case(
            CaseRun(
                task_id=task['id'],
                case_id=case['id'],
                compiler=task['compiler'],
                opt_level=task['opt_level'],
                warmups=task['warmups'],
                repeats=task['repeats'],
                timeout_seconds=task['timeout_seconds'],
                cpu=task['cpu'],
                source=source,
                input_data=input_data,
                expected_output=expected_output,
                artifact_dir=artifact_dir,
                on_running=mark_running,
            )
        )
    )
    app.state.current_execution = execution
    try:
        result = await asyncio.wait_for(execution, task['timeout_seconds'])
    except TimeoutError:
        result = {
            'status': 'TLE',
            'samples': [],
            'warmup_samples': [],
            'error': f'timeout after {task["timeout_seconds"]}s',
            'artifact_dir': str(artifact_dir),
        }
    except asyncio.CancelledError:
        if app.state.shutting_down:
            raise
        return
    finally:
        app.state.current_execution = None
        app.state.current_case_id = None

    samples = result['samples']
    warmup_samples = result['warmup_samples']
    finished_at = time.time()
    with connect_db(app.state.database_path) as connection:
        content_hashes = [
            put_blob(connection, value)
            for value in (
                result.get('compile_stdout'),
                result.get('compile_stderr'),
                result.get('stdout'),
                result.get('stderr'),
                result.get('expected_output', expected_output),
                result.get('actual_output'),
                result.get('error'),
            )
        ]
        connection.execute(
            """
      UPDATE task_cases SET status = ?, finished_at = ?, compile_stdout_blob_hash = ?,
        compile_stderr_blob_hash = ?, compile_command = ?, run_command = ?, remote_path = ?,
        stdout_blob_hash = ?, stderr_blob_hash = ?, returncode = ?,
        expected_output_blob_hash = ?, actual_output_blob_hash = ?,
        warmup_samples_json = ?, samples_json = ?, median_ms = ?, min_ms = ?, max_ms = ?,
        error_blob_hash = ?, artifact_dir = ? WHERE id = ?
      """,
            (
                result['status'],
                finished_at,
                content_hashes[0],
                content_hashes[1],
                result.get('compile_command'),
                result.get('run_command'),
                result.get('remote_path'),
                content_hashes[2],
                content_hashes[3],
                result.get('returncode'),
                content_hashes[4],
                content_hashes[5],
                json.dumps(warmup_samples),
                json.dumps(samples),
                result.get('median_ms', statistics.median(samples) if samples else None),
                result.get('min_ms', min(samples) if samples else None),
                result.get('max_ms', max(samples) if samples else None),
                content_hashes[6],
                result.get('artifact_dir', str(artifact_dir)),
                case['id'],
            ),
        )
    await emit_event(
        app,
        'log.append',
        task['id'],
        case['id'],
        {
            'source': 'case',
            'level': 'error' if result.get('error') else 'info',
            'message': result.get('error') or f'{len(samples)} performance samples collected',
        },
    )
    await emit_event(
        app,
        'case.completed',
        task['id'],
        case['id'],
        {'status': result['status'], 'median_ms': statistics.median(samples) if samples else None},
    )


async def run_task(app: FastAPI, task_id: str) -> None:
    with connect_db(app.state.database_path) as connection:
        task = connection.execute('SELECT * FROM tasks WHERE id = ?', (task_id,)).fetchone()
        if task['status'] == 'CANCELLED':
            return
        started_at = time.time()
        connection.execute(
            "UPDATE tasks SET status = 'RUNNING', started_at = ? WHERE id = ?",
            (started_at, task_id),
        )
    app.state.current_task_id = task_id
    await emit_event(app, 'task.started', task_id)

    try:
        command, stdout, stderr = await prepare_toolchain(task['compiler'], task['timeout_seconds'])
    except (OSError, TimeoutError) as error:
        finished_at = time.time()
        with connect_db(app.state.database_path) as connection:
            error_hash = put_blob(connection, str(error))
            connection.execute(
                "UPDATE tasks SET status = 'ERROR', finished_at = ? WHERE id = ?",
                (finished_at, task_id),
            )
            connection.execute(
                """
        UPDATE task_cases SET status = 'ERROR', finished_at = ?, error_blob_hash = ?
        WHERE task_id = ? AND status = 'QUEUED'
        """,
                (finished_at, error_hash, task_id),
            )
        app.state.current_task_id = None
        await emit_event(app, 'task.error', task_id, payload={'message': str(error)})
        return
    await emit_event(
        app,
        'log.append',
        task_id,
        payload={
            'source': 'toolchain',
            'level': 'info',
            'message': command,
            'stdout': stdout,
            'stderr': stderr,
        },
    )

    with connect_db(app.state.database_path) as connection:
        cases = connection.execute(
            'SELECT * FROM task_cases WHERE task_id = ? ORDER BY position', (task_id,)
        ).fetchall()

    config = BoardConfig(host=task['board_host'], port=task['board_port'], cpu=task['cpu'])
    async with BoardClient(config) as board:
        for case in cases:
            with connect_db(app.state.database_path) as connection:
                task = connection.execute('SELECT * FROM tasks WHERE id = ?', (task_id,)).fetchone()
                current_case = connection.execute(
                    'SELECT status FROM task_cases WHERE id = ?', (case['id'],)
                ).fetchone()
            if task['status'] == 'CANCELLED':
                break
            if current_case['status'] != 'QUEUED':
                continue
            try:
                await run_case(app, board, task, case)
            except (OSError, httpx2.HTTPError) as error:
                finished_at = time.time()
                with connect_db(app.state.database_path) as connection:
                    error_hash = put_blob(connection, str(error))
                    connection.execute(
                        """
                        UPDATE task_cases SET status = 'ERROR', finished_at = ?, error_blob_hash = ?
                        WHERE task_id = ? AND status IN ('QUEUED', 'COMP', 'RUN')
                        """,
                        (finished_at, error_hash, task_id),
                    )
                    connection.execute(
                        "UPDATE tasks SET status = 'ERROR', finished_at = ? WHERE id = ?",
                        (finished_at, task_id),
                    )
                app.state.current_execution = None
                app.state.current_case_id = None
                app.state.current_task_id = None
                await emit_event(
                    app,
                    'task.error',
                    task_id,
                    case['id'],
                    {'message': str(error)},
                )
                return

    with connect_db(app.state.database_path) as connection:
        task_status = connection.execute(
            'SELECT status FROM tasks WHERE id = ?', (task_id,)
        ).fetchone()[0]
        statuses = [
            row[0]
            for row in connection.execute(
                'SELECT status FROM task_cases WHERE task_id = ? ORDER BY position', (task_id,)
            )
        ]
        was_cancelled = task_status == 'CANCELLED'
        if not was_cancelled:
            task_status = (
                'CANCELLED' if all(status == 'CANCELLED' for status in statuses) else 'COMPLETE'
            )
            connection.execute(
                'UPDATE tasks SET status = ?, finished_at = ? WHERE id = ?',
                (task_status, time.time(), task_id),
            )
    app.state.current_task_id = None
    if not was_cancelled:
        await emit_event(app, f'task.{task_status.lower()}', task_id)


async def queue_worker(app: FastAPI) -> None:
    while True:
        task_id = await app.state.task_queue.get()
        try:
            await run_task(app, task_id)
        except Exception as error:  # noqa: BLE001
            finished_at = time.time()
            with connect_db(app.state.database_path) as connection:
                error_hash = put_blob(connection, str(error))
                connection.execute(
                    "UPDATE tasks SET status = 'ERROR', finished_at = ? WHERE id = ?",
                    (finished_at, task_id),
                )
                connection.execute(
                    """
                    UPDATE task_cases SET status = 'ERROR', finished_at = ?, error_blob_hash = ?
                    WHERE task_id = ? AND status IN ('QUEUED', 'COMP', 'RUN')
                    """,
                    (finished_at, error_hash, task_id),
                )
            app.state.current_execution = None
            app.state.current_case_id = None
            app.state.current_task_id = None
            await emit_event(app, 'task.error', task_id, payload={'message': str(error)})
        finally:
            app.state.task_queue.task_done()


def snapshot(app: FastAPI) -> dict[str, Any]:
    return {
        'board': public_board(app.state.board_config),
        'tasks': ordered_tasks(app.state.database_path),
    }


async def enqueue_task(
    app: FastAPI,
    config: dict[str, Any],
    cases: list[dict[str, Any]],
) -> dict[str, Any]:
    revision, dirty = git_revision()
    task_id = uuid.uuid4().hex
    created_at = time.time()
    with connect_db(app.state.database_path) as connection:
        cursor = connection.execute(
            """
      INSERT INTO tasks (
        id, git_hash, dirty, compiler, opt_level, warmups, repeats,
        timeout_seconds, board_host, board_port, cpu, status, created_at
      ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'QUEUED', ?)
      """,
            (
                task_id,
                revision,
                dirty,
                config['compiler'],
                config['opt_level'],
                config['warmups'],
                config['repeats'],
                config['timeout_seconds'],
                config['board_host'],
                config['board_port'],
                config['cpu'],
                created_at,
            ),
        )
        number = cursor.lastrowid
        for position, case in enumerate(cases):
            source_hash = put_blob(connection, case['source'])
            input_hash = put_blob(connection, case.get('input'))
            expected_hash = put_blob(connection, case.get('expected'))
            connection.execute(
                """
        INSERT INTO task_cases (
          id, task_id, position, case_id, suite, name, source_path,
          source_blob_hash, input_blob_hash, expected_blob_hash,
          expected_output_blob_hash, status
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'QUEUED')
        """,
                (
                    uuid.uuid4().hex,
                    task_id,
                    position,
                    case['case_id'],
                    case['suite'],
                    case['name'],
                    case['source_path'],
                    source_hash,
                    input_hash,
                    expected_hash,
                    expected_hash,
                ),
            )
    await emit_event(app, 'task.created', task_id, payload={'number': number})
    await emit_event(app, 'task.queued', task_id)
    await app.state.task_queue.put(task_id)
    return task_record(app.state.database_path, task_id)


def create_app(database_path: Path = DB_PATH) -> FastAPI:
    @asynccontextmanager
    async def lifespan(app: FastAPI):
        init_db(database_path)
        app.state.database_path = database_path
        app.state.artifact_root = database_path.parent / 'tasks'
        with connect_db(database_path) as connection:
            saved_board = connection.execute('SELECT * FROM board_config WHERE id = 1').fetchone()
        app.state.board_config = BoardUpdate(
            host=saved_board['host'],
            port=saved_board['port'],
            cpu=saved_board['cpu'],
        )
        app.state.task_queue = asyncio.Queue()
        app.state.event_condition = asyncio.Condition()
        app.state.current_task_id = None
        app.state.current_case_id = None
        app.state.current_execution = None
        app.state.shutting_down = False
        worker = asyncio.create_task(queue_worker(app))
        yield
        app.state.shutting_down = True
        worker.cancel()
        with suppress(asyncio.CancelledError):
            await worker

    app = FastAPI(title='ARM Bench', lifespan=lifespan)

    @app.get('/api/cases')
    async def cases_api() -> list[dict[str, Any]]:
        return [asdict(case) for case in list_cases()]

    @app.post('/api/cases', status_code=201)
    async def create_case(body: CaseWrite, request: Request) -> dict[str, Any]:
        source = target_case_path(body.suite, body.name)
        if source.exists():
            raise HTTPException(409, 'Test case already exists')
        write_case(source, body)
        record = case_contents(source)
        await emit_event(
            request.app, 'cases.changed', payload={'action': 'created', 'id': record['id']}
        )
        return record

    @app.get('/api/cases/{case_id:path}')
    async def get_case(case_id: str) -> dict[str, Any]:
        return case_contents(resolve_case(case_id))

    @app.put('/api/cases/{case_id:path}')
    async def update_case(case_id: str, body: CaseWrite, request: Request) -> dict[str, Any]:
        current = resolve_case(case_id)
        destination = target_case_path(body.suite, body.name)
        if destination != current and destination.exists():
            raise HTTPException(409, 'Test case already exists')
        write_case(destination, body)
        if destination != current:
            delete_case_files(current)
        record = case_contents(destination)
        await emit_event(
            request.app,
            'cases.changed',
            payload={'action': 'updated', 'previous_id': case_id, 'id': record['id']},
        )
        return record

    @app.delete('/api/cases/{case_id:path}', status_code=204)
    async def delete_case(case_id: str, request: Request) -> None:
        source = resolve_case(case_id)
        delete_case_files(source)
        await emit_event(
            request.app,
            'cases.changed',
            payload={'action': 'deleted', 'id': case_id},
        )

    @app.get('/api/board')
    async def get_board(request: Request) -> dict[str, Any]:
        return public_board(request.app.state.board_config)

    @app.put('/api/board')
    async def update_board(settings: BoardUpdate, request: Request) -> dict[str, Any]:
        request.app.state.board_config = settings
        with connect_db(database(request)) as connection:
            connection.execute(
                'UPDATE board_config SET host = ?, port = ?, cpu = ? WHERE id = 1',
                (settings.host, settings.port, settings.cpu),
            )
        await emit_event(request.app, 'board.configured', payload=public_board(settings))
        return public_board(settings)

    @app.post('/api/board/test')
    async def test_board(request: Request) -> dict[str, Any]:
        config = request.app.state.board_config
        try:
            async with BoardClient(
                BoardConfig(host=config.host, port=config.port, cpu=config.cpu)
            ) as board:
                status = await board.status()
        except (OSError, httpx2.HTTPError) as error:
            raise HTTPException(502, str(error)) from error
        await emit_event(request.app, 'board.status', payload=status)
        return {**public_board(request.app.state.board_config), **status}

    @app.post('/api/tasks', status_code=201)
    async def create_task(body: TaskCreate, request: Request) -> dict[str, Any]:
        if not body.cases:
            raise HTTPException(400, 'Select at least one case')
        sources = [resolve_case(case_id) for case_id in body.cases]
        board = request.app.state.board_config
        cases = [
            {
                'case_id': case_id,
                'suite': case_id.split('/', 1)[0],
                'name': source.stem,
                'source_path': str(source),
                'source': read_case_file(source),
                'input': read_case_file(source.with_suffix('.in'))
                if source.with_suffix('.in').exists()
                else None,
                'expected': read_case_file(source.with_suffix('.out'))
                if source.with_suffix('.out').exists()
                else None,
            }
            for case_id, source in zip(body.cases, sources, strict=True)
        ]
        return await enqueue_task(
            request.app,
            {
                'compiler': body.compiler,
                'opt_level': body.opt_level,
                'warmups': body.warmups,
                'repeats': body.repeats,
                'timeout_seconds': body.timeout_seconds,
                'board_host': board.host,
                'board_port': board.port,
                'cpu': board.cpu,
            },
            cases,
        )

    @app.get('/api/tasks')
    async def list_tasks(request: Request) -> list[dict[str, Any]]:
        return ordered_tasks(database(request))

    @app.get('/api/tasks/{task_id}')
    async def get_task(task_id: str, request: Request) -> dict[str, Any]:
        return task_record(database(request), task_id)

    @app.get('/api/tasks/{task_id}/cases/{task_case_id}')
    async def get_task_case(task_id: str, task_case_id: str, request: Request) -> dict[str, Any]:
        with connect_db(database(request)) as connection:
            case = connection.execute(
                f'SELECT {CASE_DETAIL_COLUMNS} FROM task_cases WHERE id = ? AND task_id = ?',
                (task_case_id, task_id),
            ).fetchone()
        if case is None:
            raise HTTPException(404, 'Unknown task case')
        record = case_detail_record(case)
        record['baseline'] = baseline_records(database(request)).get(record['case_id'])
        return record

    @app.get('/api/tasks/{task_id}/cases/{task_case_id}/content/{kind}')
    async def case_content(
        task_id: str,
        task_case_id: str,
        kind: Literal[
            'input',
            'compile_stdout',
            'compile_stderr',
            'stdout',
            'stderr',
            'expected_output',
            'actual_output',
            'error',
        ],
        request: Request,
        download: bool = False,
    ) -> Response:
        hash_column, filename = CASE_CONTENT_FIELDS[kind]
        with connect_db(database(request)) as connection:
            case = connection.execute(
                f'SELECT {hash_column} FROM task_cases WHERE id = ? AND task_id = ?',
                (task_case_id, task_id),
            ).fetchone()
            if case is None:
                raise HTTPException(404, 'Unknown task case')
            digest = case[hash_column]
            if digest is None:
                content = ''
                size = 0
            elif download:
                blob = connection.execute(
                    'SELECT data FROM content_blobs WHERE hash = ?', (digest,)
                ).fetchone()
                content = bytes(blob['data']).decode()
                size = len(blob['data'])
            else:
                blob = connection.execute(
                    """
                    SELECT substr(CAST(data AS TEXT), 1, 65536) AS content, length(data) AS size
                    FROM content_blobs WHERE hash = ?
                    """,
                    (digest,),
                ).fetchone()
                content = blob['content']
                size = blob['size']
        headers = {'X-Content-Truncated': 'true'} if len(content.encode()) < size else {}
        if download:
            headers['Content-Disposition'] = f'attachment; filename="{filename}"'
        return Response(content, media_type='text/plain; charset=utf-8', headers=headers)

    @app.get('/api/queue')
    async def get_queue(request: Request) -> list[dict[str, Any]]:
        config = request.app.state.board_config
        try:
            async with BoardClient(
                BoardConfig(host=config.host, port=config.port, cpu=config.cpu)
            ) as board:
                remote_jobs = await board.queue()
        except httpx2.HTTPError as error:
            raise HTTPException(502, str(error)) from error
        remote_order = {job['id']: position for position, job in enumerate(remote_jobs)}
        tasks = queue_records(database(request))
        return sorted(
            tasks,
            key=lambda task: min(
                (remote_order[case['id']] for case in task['cases'] if case['id'] in remote_order),
                default=len(remote_order) + task['number'],
            ),
        )

    @app.post('/api/tasks/{task_id}/cancel')
    async def cancel_task(task_id: str, request: Request) -> dict[str, Any]:
        with connect_db(database(request)) as connection:
            task = connection.execute(
                'SELECT status FROM tasks WHERE id = ?', (task_id,)
            ).fetchone()
            if task is None:
                raise HTTPException(404, 'Unknown task')
            if task['status'] in TERMINAL_TASK_STATUSES:
                return task_record(database(request), task_id)
            job_ids = [
                row['id']
                for row in connection.execute(
                    'SELECT id FROM task_cases WHERE task_id = ? '
                    "AND status IN ('QUEUED', 'COMP', 'RUN')",
                    (task_id,),
                )
            ]
        config = request.app.state.board_config
        try:
            async with BoardClient(
                BoardConfig(host=config.host, port=config.port, cpu=config.cpu)
            ) as board:
                for job_id in job_ids:
                    await board.cancel_job(job_id)
        except httpx2.HTTPError as error:
            raise HTTPException(502, str(error)) from error
        with connect_db(database(request)) as connection:
            now = time.time()
            connection.execute(
                "UPDATE tasks SET status = 'CANCELLED', finished_at = ? WHERE id = ?",
                (now, task_id),
            )
            connection.execute(
                """
        UPDATE task_cases SET status = 'CANCELLED', finished_at = ?
        WHERE task_id = ? AND status IN ('QUEUED', 'COMP', 'RUN')
        """,
                (now, task_id),
            )
        if request.app.state.current_task_id == task_id and request.app.state.current_execution:
            request.app.state.current_execution.cancel()
        await emit_event(request.app, 'task.cancelled', task_id)
        return task_record(database(request), task_id)

    @app.post('/api/tasks/{task_id}/cases/{task_case_id}/cancel')
    async def cancel_case(task_id: str, task_case_id: str, request: Request) -> dict[str, Any]:
        with connect_db(database(request)) as connection:
            case = connection.execute(
                'SELECT * FROM task_cases WHERE id = ? AND task_id = ?', (task_case_id, task_id)
            ).fetchone()
            if case is None:
                raise HTTPException(404, 'Unknown task case')
            if case['status'] in TERMINAL_CASE_STATUSES:
                return task_record(database(request), task_id)
        config = request.app.state.board_config
        try:
            async with BoardClient(
                BoardConfig(host=config.host, port=config.port, cpu=config.cpu)
            ) as board:
                await board.cancel_job(task_case_id)
        except httpx2.HTTPError as error:
            raise HTTPException(502, str(error)) from error
        with connect_db(database(request)) as connection:
            connection.execute(
                "UPDATE task_cases SET status = 'CANCELLED', finished_at = ? WHERE id = ?",
                (time.time(), task_case_id),
            )
            remaining = connection.execute(
                "SELECT COUNT(*) FROM task_cases WHERE task_id = ? AND status != 'CANCELLED'",
                (task_id,),
            ).fetchone()[0]
            task_status = connection.execute(
                'SELECT status FROM tasks WHERE id = ?', (task_id,)
            ).fetchone()[0]
            cancel_task_too = remaining == 0 and task_status == 'QUEUED'
            if cancel_task_too:
                connection.execute(
                    "UPDATE tasks SET status = 'CANCELLED', finished_at = ? WHERE id = ?",
                    (time.time(), task_id),
                )
        if (
            request.app.state.current_case_id == task_case_id
            and request.app.state.current_execution
        ):
            request.app.state.current_execution.cancel()
        await emit_event(request.app, 'case.cancelled', task_id, task_case_id)
        if cancel_task_too:
            await emit_event(request.app, 'task.cancelled', task_id)
        return task_record(database(request), task_id)

    @app.post('/api/tasks/{task_id}/retry')
    async def retry_task(task_id: str, request: Request) -> dict[str, Any]:
        with connect_db(database(request)) as connection:
            task = connection.execute('SELECT * FROM tasks WHERE id = ?', (task_id,)).fetchone()
            if task is None:
                raise HTTPException(404, 'Unknown task')
            if (
                task['status'] not in TERMINAL_TASK_STATUSES
                or request.app.state.current_task_id == task_id
            ):
                raise HTTPException(409, 'Stop the task before retrying it')
            cases = connection.execute(
                'SELECT id, status FROM task_cases WHERE task_id = ? ORDER BY position',
                (task_id,),
            ).fetchall()
            case_ids = [case['id'] for case in cases if case['status'] in RETRYABLE_CASE_STATUSES]
        if not case_ids:
            raise HTTPException(409, 'This task has no failed cases to retry')
        try:
            async with BoardClient(
                BoardConfig(host=task['board_host'], port=task['board_port'], cpu=task['cpu'])
            ) as board:
                for case_id in case_ids:
                    await board.cancel_job(case_id)
        except httpx2.HTTPError as error:
            raise HTTPException(502, str(error)) from error
        for case_id in case_ids:
            artifact_dir = request.app.state.artifact_root / task_id / case_id
            if artifact_dir.exists():
                shutil.rmtree(artifact_dir)
        with connect_db(database(request)) as connection:
            reset_task_cases(connection, case_ids)
            connection.execute(
                """
                UPDATE tasks SET status = 'QUEUED', started_at = NULL, finished_at = NULL
                WHERE id = ?
                """,
                (task_id,),
            )
        await emit_event(
            request.app,
            'task.retried',
            task_id,
            payload={'case_ids': case_ids},
        )
        await emit_event(request.app, 'task.queued', task_id)
        await request.app.state.task_queue.put(task_id)
        return task_record(database(request), task_id)

    @app.post('/api/tasks/{task_id}/cases/{task_case_id}/retry')
    async def retry_case(task_id: str, task_case_id: str, request: Request) -> dict[str, Any]:
        with connect_db(database(request)) as connection:
            task = connection.execute('SELECT * FROM tasks WHERE id = ?', (task_id,)).fetchone()
            case = connection.execute(
                'SELECT id, status FROM task_cases WHERE id = ? AND task_id = ?',
                (task_case_id, task_id),
            ).fetchone()
            if task is None or case is None:
                raise HTTPException(404, 'Unknown task case')
            if (
                task['status'] not in TERMINAL_TASK_STATUSES
                or request.app.state.current_task_id == task_id
                or case['status'] not in TERMINAL_CASE_STATUSES
            ):
                raise HTTPException(409, 'Stop the task before retrying this case')
        try:
            async with BoardClient(
                BoardConfig(host=task['board_host'], port=task['board_port'], cpu=task['cpu'])
            ) as board:
                await board.cancel_job(task_case_id)
        except httpx2.HTTPError as error:
            raise HTTPException(502, str(error)) from error
        artifact_dir = request.app.state.artifact_root / task_id / task_case_id
        if artifact_dir.exists():
            shutil.rmtree(artifact_dir)
        with connect_db(database(request)) as connection:
            reset_task_cases(connection, [task_case_id])
            connection.execute(
                """
                UPDATE tasks SET status = 'QUEUED', started_at = NULL, finished_at = NULL
                WHERE id = ?
                """,
                (task_id,),
            )
        await emit_event(
            request.app,
            'case.retried',
            task_id,
            task_case_id,
            {'case_id': task_case_id},
        )
        await emit_event(request.app, 'task.queued', task_id)
        await request.app.state.task_queue.put(task_id)
        return task_record(database(request), task_id)

    @app.put('/api/tasks/{task_id}/cases/{task_case_id}/baseline')
    async def set_case_baseline(
        task_id: str, task_case_id: str, request: Request
    ) -> dict[str, Any]:
        with connect_db(database(request)) as connection:
            case = connection.execute(
                'SELECT id, case_id, status FROM task_cases WHERE id = ? AND task_id = ?',
                (task_case_id, task_id),
            ).fetchone()
            if case is None:
                raise HTTPException(404, 'Unknown task case')
            if case['status'] != 'PASS':
                raise HTTPException(409, 'Only PASS results can be baselines')
            connection.execute(
                """
        INSERT INTO baselines (case_id, task_case_id) VALUES (?, ?)
        ON CONFLICT(case_id) DO UPDATE SET task_case_id = excluded.task_case_id
        """,
                (case['case_id'], task_case_id),
            )
        await emit_event(
            request.app,
            'baseline.changed',
            task_id,
            task_case_id,
            {'case_id': case['case_id']},
        )
        return task_record(database(request), task_id)

    @app.put('/api/tasks/{task_id}/baseline')
    async def set_task_baselines(task_id: str, request: Request) -> dict[str, Any]:
        with connect_db(database(request)) as connection:
            task = connection.execute('SELECT id FROM tasks WHERE id = ?', (task_id,)).fetchone()
            if task is None:
                raise HTTPException(404, 'Unknown task')
            cases = connection.execute(
                "SELECT id, case_id FROM task_cases WHERE task_id = ? AND status = 'PASS'",
                (task_id,),
            ).fetchall()
            for case in cases:
                connection.execute(
                    """
          INSERT INTO baselines (case_id, task_case_id) VALUES (?, ?)
          ON CONFLICT(case_id) DO UPDATE SET task_case_id = excluded.task_case_id
          """,
                    (case['case_id'], case['id']),
                )
        await emit_event(
            request.app,
            'baseline.changed',
            task_id,
            payload={'count': len(cases)},
        )
        return task_record(database(request), task_id)

    @app.delete('/api/tasks/{task_id}', status_code=204)
    async def delete_task(task_id: str, request: Request) -> None:
        with connect_db(database(request)) as connection:
            task = connection.execute(
                'SELECT number, status FROM tasks WHERE id = ?', (task_id,)
            ).fetchone()
            if task is None:
                raise HTTPException(404, 'Unknown task')
            if task['status'] not in TERMINAL_TASK_STATUSES:
                raise HTTPException(409, 'Stop the task before deleting it')
            connection.execute('DELETE FROM events WHERE task_id = ?', (task_id,))
            connection.execute('DELETE FROM tasks WHERE id = ?', (task_id,))
            delete_unused_blobs(connection)
        artifact_dir = request.app.state.artifact_root / task_id
        if artifact_dir.exists():
            shutil.rmtree(artifact_dir)
        await emit_event(
            request.app,
            'task.deleted',
            payload={'task_id': task_id, 'number': task['number']},
        )

    @app.get('/api/tasks/{task_id}/events')
    async def task_events(task_id: str, request: Request) -> list[dict[str, Any]]:
        with connect_db(database(request)) as connection:
            exists = connection.execute('SELECT 1 FROM tasks WHERE id = ?', (task_id,)).fetchone()
        if exists is None:
            raise HTTPException(404, 'Unknown task')
        return event_records(database(request), 0, task_id)

    @app.get('/api/tasks/{task_id}/cases/{task_case_id}/artifacts/{kind}')
    async def case_artifact(
        task_id: str,
        task_case_id: str,
        kind: Literal['assembly', 'elf', 'raana'],
        request: Request,
        download: bool = False,
    ) -> Response:
        with connect_db(database(request)) as connection:
            case = connection.execute(
                'SELECT artifact_dir FROM task_cases WHERE id = ? AND task_id = ?',
                (task_case_id, task_id),
            ).fetchone()
        if case is None:
            raise HTTPException(404, 'Unknown task case')
        filename = {'assembly': 'program.s', 'elf': 'program.elf', 'raana': 'program.raana'}[kind]
        path = Path(case['artifact_dir']) / filename
        if not path.is_file():
            raise HTTPException(404, 'Artifact is not available')
        if download or kind == 'elf':
            return FileResponse(path, filename=filename)
        size = path.stat().st_size
        with path.open('rb') as artifact:
            content = artifact.read(65536)
        headers = {'X-Content-Truncated': 'true'} if len(content) < size else {}
        return Response(content.decode(errors='replace'), media_type='text/plain', headers=headers)

    @app.websocket('/api/ws')
    async def websocket_events(websocket: WebSocket) -> None:
        await websocket.accept()
        cursor_text = websocket.query_params.get('lastSeq')
        current_seq = latest_seq(websocket.app.state.database_path)
        if cursor_text is None or int(cursor_text) > current_seq:
            cursor = current_seq
            await websocket.send_json(
                {
                    'seq': cursor,
                    'type': 'snapshot',
                    'taskId': None,
                    'caseId': None,
                    'payload': snapshot(websocket.app),
                }
            )
        else:
            cursor = int(cursor_text)
        disconnected = asyncio.create_task(wait_for_disconnect(websocket))
        try:
            while True:
                events = event_records(websocket.app.state.database_path, cursor)
                for event in events:
                    await websocket.send_json(event)
                    cursor = event['seq']
                if events:
                    continue
                changed = asyncio.create_task(wait_for_event(websocket.app, cursor))
                done, _ = await asyncio.wait(
                    {changed, disconnected}, return_when=asyncio.FIRST_COMPLETED
                )
                if disconnected in done:
                    changed.cancel()
                    return
        finally:
            disconnected.cancel()

    return app


app = create_app()


def main() -> None:
    import uvicorn

    uvicorn.run('arm_bench.server:app', host='127.0.0.1', port=8765, reload=False)


if __name__ == '__main__':
    main()
