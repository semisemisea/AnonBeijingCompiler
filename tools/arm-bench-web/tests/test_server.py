import asyncio
import base64
import hashlib
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path

import httpx2
import pytest
from fastapi import HTTPException
from fastapi.testclient import TestClient

from arm_bench import resync_cases, server
from arm_bench.board import combined_output

CASE_A = 'functional/00_main.sy'
CASE_B = 'functional/01_var_defn2.sy'
BENCHD = Path(__file__).resolve().parents[1] / 'board/soyo-benchd'


def install_test_board(
    monkeypatch,
    statuses: list[str] | None = None,
    delay: float = 0.03,
    failures: list[str] | None = None,
):
    pending_statuses = iter(statuses or ['PASS'] * 40)
    pending_failures = iter(failures or [])

    class TestBoard:
        def __init__(self, config):
            self.config = config

        async def __aenter__(self):
            return self

        async def __aexit__(self, *_args):
            return None

        async def queue(self):
            return []

        async def cancel_job(self, _job_id):
            return None

        async def status(self):
            return {
                'online': True,
                'architecture': 'aarch64',
                'cpus': '0-3',
                'frequency_khz': 1_200_000,
                'temperature_millidegrees': 42_000,
                'latency_ms': 3.0,
            }

        async def run_case(self, run):
            await run.on_running()
            await asyncio.sleep(delay * run.repeats)
            try:
                failure = next(pending_failures)
            except StopIteration:
                pass
            else:
                raise OSError(failure)
            status = next(pending_statuses)
            run.artifact_dir.mkdir(parents=True)
            (run.artifact_dir / 'program.s').write_text('.text\n.global main\nmain:\n  ret\n')
            (run.artifact_dir / 'program.elf').write_bytes(b'ARM-BENCH-TEST-ELF\n')
            (run.artifact_dir / 'program.raana').write_text('function main()\n')
            expected = run.expected_output
            actual = expected if status == 'PASS' else 'wrong output\n0\n'
            error = {
                'CE': 'compiler error',
                'RE': 'runtime error',
                'TLE': 'execution timed out',
                'ERROR': 'board connection interrupted',
            }.get(status)
            samples = [10.0 + index for index in range(run.repeats)] if status == 'PASS' else []
            return {
                'status': status,
                'compile_command': f'test-cc {run.compiler} -O{run.opt_level} program.sy',
                'run_command': 'POST http://bench.local:8766/jobs',
                'remote_path': f'/jobs/{run.case_id}',
                'compile_stdout': 'compile complete\n' if status != 'CE' else '',
                'compile_stderr': error if status == 'CE' else '',
                'stdout': '' if status in {'CE', 'TLE', 'ERROR'} else actual,
                'stderr': error if status == 'RE' else '',
                'returncode': 0 if status in {'PASS', 'WA'} else None,
                'expected_output': expected,
                'actual_output': actual,
                'warmup_samples': [],
                'samples': samples,
                'error': error,
                'artifact_dir': str(run.artifact_dir),
            }

    async def prepare_toolchain(compiler, timeout):
        return f'prepare {compiler} {timeout}', '', ''

    monkeypatch.setattr(server, 'BoardClient', TestBoard)
    monkeypatch.setattr(server, 'prepare_toolchain', prepare_toolchain)


def wait_for_status(client: TestClient, task_id: str, status: str) -> dict:
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        task = client.get(f'/api/tasks/{task_id}').json()
        if task['status'] == status:
            return task
        time.sleep(0.01)
    raise AssertionError(f'task {task_id} did not reach {status}')


def case_detail(client: TestClient, task_id: str, case_id: str) -> dict:
    return client.get(f'/api/tasks/{task_id}/cases/{case_id}').json()


def case_content(client: TestClient, task_id: str, case_id: str, kind: str) -> str:
    return client.get(f'/api/tasks/{task_id}/cases/{case_id}/content/{kind}').text


@pytest.fixture
def client(tmp_path: Path, monkeypatch):
    install_test_board(monkeypatch)
    app = server.create_app(tmp_path / 'arm-bench.sqlite3')
    with TestClient(app) as test_client:
        yield test_client


def create_task(client: TestClient, cases: list[str], repeats: int = 1) -> dict:
    response = client.post(
        '/api/tasks',
        json={
            'compiler': 'SOYO',
            'opt_level': 2,
            'cases': cases,
            'warmups': 0,
            'repeats': repeats,
            'timeout_seconds': 10,
        },
    )
    assert response.status_code == 201
    return response.json()


@pytest.fixture
def benchd():
    process = subprocess.Popen(
        [sys.executable, str(BENCHD), '--host', '127.0.0.1', '--port', '0'],
        stdout=subprocess.PIPE,
        text=True,
    )
    assert process.stdout is not None
    port = int(process.stdout.readline().rsplit(':', 1)[1])
    try:
        yield httpx2.Client(base_url=f'http://127.0.0.1:{port}')
    finally:
        process.terminate()
        process.wait(timeout=2)


def submit_job(client, job_id: str, program: bytes, timeout: float = 2):
    return client.post(
        '/jobs',
        json={
            'id': job_id,
            'cpu': min(os.sched_getaffinity(0)),
            'timeout': timeout,
            'warmups': 0,
            'repeats': 1,
            'elf': base64.b64encode(program).decode(),
            'input': base64.b64encode(b'hello\n').decode(),
        },
    )


def wait_for_job(client, job_id: str):
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        job = client.get(f'/jobs/{job_id}').json()
        if job['status'] in {'COMPLETE', 'ERROR', 'CANCELLED'}:
            return job
        time.sleep(0.01)
    raise AssertionError(f'job {job_id} did not finish')


def test_board_service_runs_jobs_and_reports_queue(benchd):
    response = submit_job(benchd, 'job-1', b'#!/bin/sh\ncat\nexit 7\n')
    job = wait_for_job(benchd, 'job-1')

    assert response.status_code == 202
    assert job['status'] == 'COMPLETE'
    assert job['result']['stdout'] == 'hello\n'
    assert job['result']['returncode'] == 7
    assert job['result']['elapsed_ns'] > 0
    assert job['result']['clock'] == 'CLOCK_MONOTONIC_RAW'
    assert benchd.get('/queue').json() == []


def test_board_service_times_out_and_cancels_jobs(benchd):
    submit_job(benchd, 'timeout', b'#!/bin/sh\nsleep 10\n', timeout=0.02)
    timed_out = wait_for_job(benchd, 'timeout')
    submit_job(benchd, 'cancel', b'#!/bin/sh\nsleep 10\n')
    assert benchd.delete('/jobs/cancel').json()['cancelled'] is True
    cancelled = wait_for_job(benchd, 'cancel')

    assert timed_out['result']['timed_out'] is True
    assert timed_out['result']['returncode'] == 137
    assert cancelled['status'] == 'CANCELLED'


def test_repository_cases_are_discovered():
    cases = server.list_cases()
    assert len(cases) == 212
    assert {case.suite for case in cases} == {'tensor', 'functional', 'h_functional', 'perf'}


def test_resolve_case_rejects_path_traversal():
    with pytest.raises(HTTPException) as error:
        server.resolve_case('../Cargo.toml')
    assert error.value.status_code == 404


def test_resolve_case_returns_sy_source():
    source = server.resolve_case(CASE_A)
    assert source == Path(server.TESTS / CASE_A)


def test_test_cases_can_be_created_edited_moved_and_deleted(tmp_path: Path, monkeypatch):
    tests_root = tmp_path / 'cases'
    for suite in server.CASE_SUITES:
        (tests_root / suite).mkdir(parents=True)
    monkeypatch.setattr(server, 'TESTS', tests_root)
    install_test_board(monkeypatch)
    app = server.create_app(tmp_path / 'arm-bench.sqlite3')

    with TestClient(app) as client:
        created = client.post(
            '/api/cases',
            json={
                'suite': 'functional',
                'name': 'custom.sy',
                'source': 'int main() { return 0; }\n',
                'input': '1\n',
                'expected': '0\n',
            },
        )
        assert created.status_code == 201
        assert created.json()['id'] == 'functional/custom.sy'

        updated = client.put(
            '/api/cases/functional/custom.sy',
            json={
                'suite': 'perf',
                'name': 'renamed',
                'source': 'int main() { return 3; }\n',
                'input': None,
                'expected': '3\n',
            },
        )
        assert updated.status_code == 200
        assert updated.json()['id'] == 'perf/renamed.sy'
        assert client.get('/api/cases/functional/custom.sy').status_code == 404
        assert client.get('/api/cases/perf/renamed.sy').json()['source'].endswith('return 3; }\n')
        assert [case['id'] for case in client.get('/api/cases').json()] == ['perf/renamed.sy']

        deleted = client.delete('/api/cases/perf/renamed.sy')
        assert deleted.status_code == 204
        assert client.get('/api/cases').json() == []

    assert not (tests_root / 'functional/custom.sy').exists()
    assert not (tests_root / 'functional/custom.in').exists()
    assert not (tests_root / 'perf/renamed.sy').exists()
    assert not (tests_root / 'perf/renamed.out').exists()


def test_case_files_are_read_byte_exact_preserving_crlf(tmp_path: Path, monkeypatch):
    """CRLF in .out must survive snapshotting (Path.read_text would strip it)."""
    tests_root = tmp_path / 'cases'
    (tests_root / 'functional').mkdir(parents=True)
    raw_expected = b'The quick brown fox.\r\n0\n'
    (tests_root / 'functional' / 'crlf_case.sy').write_bytes(b'int main() { return 0; }\n')
    (tests_root / 'functional' / 'crlf_case.out').write_bytes(raw_expected)
    monkeypatch.setattr(server, 'TESTS', tests_root)
    install_test_board(monkeypatch)
    app = server.create_app(tmp_path / 'arm-bench.sqlite3')

    with TestClient(app) as client:
        fetched = client.get('/api/cases/functional/crlf_case.sy').json()
        assert fetched['expected'] == raw_expected.decode()
        assert '\r\n' in fetched['expected']

        task = create_task(client, ['functional/crlf_case.sy'])
        finished = wait_for_status(client, task['id'], 'COMPLETE')
        case = finished['cases'][0]
        assert case['status'] == 'PASS'

        with server.connect_db(app.state.database_path) as connection:
            blob = connection.execute(
                'SELECT data FROM content_blobs WHERE hash = '
                '(SELECT expected_blob_hash FROM task_cases WHERE id = ?)',
                (case['id'],),
            ).fetchone()['data']
        assert bytes(blob) == raw_expected

        expected_response = client.get(
            f'/api/tasks/{task["id"]}/cases/{case["id"]}/content/expected_output'
        )
        assert expected_response.content == raw_expected


def test_resync_case_snapshots_repairs_crlf_corrupted_blobs(tmp_path: Path):
    """A snapshot damaged by read_text()'s newline translation is repointed at the byte-exact blob."""
    tests_root = tmp_path / 'tests'
    (tests_root / 'functional').mkdir(parents=True)
    expected = b'answer\r\n1\n'
    (tests_root / 'functional' / 'crlf_case.sy').write_bytes(b'int main() { return 1; }\n')
    (tests_root / 'functional' / 'crlf_case.out').write_bytes(expected)

    database = tmp_path / 'arm-bench.sqlite3'
    server.init_db(database)
    with server.connect_db(database) as connection:
        connection.execute(
            'INSERT INTO tasks (id, git_hash, dirty, compiler, opt_level, warmups, repeats, '
            'timeout_seconds, board_host, board_port, cpu, status, created_at) VALUES '
            "(?, 'abc', 0, 'CLANG', 2, 0, 1, 10, 'bench.local', 8766, 0, 'COMPLETE', 0)",
            ('task-1',),
        )
        source_hash = server.put_blob(
            connection,
            (tests_root / 'functional' / 'crlf_case.sy').read_bytes().decode(),
        )
        stripped = resync_cases._universal_newlines(expected)
        corrupt_hash = hashlib.sha256(stripped).hexdigest()
        connection.execute(
            'INSERT OR IGNORE INTO content_blobs (hash, data) VALUES (?, ?)',
            (corrupt_hash, stripped),
        )
        connection.execute(
            'INSERT INTO task_cases (id, task_id, position, case_id, suite, name, source_path, '
            'source_blob_hash, input_blob_hash, expected_blob_hash, expected_output_blob_hash, '
            "status) VALUES (?, ?, 0, 'functional/crlf_case.sy', 'functional', 'crlf_case', "
            "?, ?, NULL, ?, ?, 'WA')",
            (
                'case-1',
                'task-1',
                str(tests_root / 'functional' / 'crlf_case.sy'),
                source_hash,
                corrupt_hash,
                corrupt_hash,
            ),
        )

    report = resync_cases.resync_case_snapshots(database, tests_root=tests_root)
    assert report['rows_updated'] == 1
    assert report['columns_updated']['expected_blob_hash'] == 1
    assert report['expected_output_fixed'] == 1
    with server.connect_db(database) as connection:
        row = connection.execute(
            'SELECT expected_blob_hash, expected_output_blob_hash FROM task_cases'
        ).fetchone()
        stored = connection.execute(
            'SELECT data FROM content_blobs WHERE hash = ?', (row['expected_blob_hash'],)
        ).fetchone()['data']
    assert row['expected_blob_hash'] == hashlib.sha256(expected).hexdigest()
    assert row['expected_output_blob_hash'] == row['expected_blob_hash']
    assert bytes(stored) == expected

    # Already repaired rows are left untouched on a second pass.
    again = resync_cases.resync_case_snapshots(database, tests_root=tests_root)
    assert again['rows_updated'] == 0

    # A snapshot that differs for another reason is not hijacked.
    with server.connect_db(database) as connection:
        other_hash = server.put_blob(connection, 'renamed output\n0\n')
        connection.execute(
            'UPDATE task_cases SET expected_blob_hash = ?, expected_output_blob_hash = ? '
            "WHERE id = 'case-1'",
            (other_hash, other_hash),
        )
    report = resync_cases.resync_case_snapshots(database, tests_root=tests_root)
    assert report['rows_updated'] == 0


def test_test_case_name_rejects_paths(tmp_path: Path, monkeypatch):
    monkeypatch.setattr(server, 'TESTS', tmp_path / 'cases')
    app = server.create_app(tmp_path / 'arm-bench.sqlite3')
    with TestClient(app) as client:
        response = client.post(
            '/api/cases',
            json={
                'suite': 'functional',
                'name': '../escape',
                'source': 'int main() { return 0; }\n',
            },
        )

    assert response.status_code == 400
    assert not (tmp_path / 'escape.sy').exists()


def test_board_configuration_persists(tmp_path: Path):
    database = tmp_path / 'arm-bench.sqlite3'
    app = server.create_app(database)
    with TestClient(app) as client:
        saved = client.put(
            '/api/board',
            json={
                'host': 'bench.local',
                'port': 2222,
                'cpu': 3,
            },
        ).json()

    restarted = server.create_app(database)
    with TestClient(restarted) as client:
        status = client.get('/api/board').json()

    assert (
        saved
        == status
        == {
            'host': 'bench.local',
            'port': 2222,
            'cpu': 3,
            'service': 'soyo-benchd',
        }
    )
    with server.connect_db(database) as connection:
        columns = {row['name'] for row in connection.execute('PRAGMA table_info(board_config)')}
    assert 'password' not in columns


def test_board_status_is_reported(client: TestClient):
    status = client.post('/api/board/test').json()

    assert status['online'] is True
    assert status['architecture'] == 'aarch64'
    assert status['frequency_khz'] == 1_200_000
    assert status['temperature_millidegrees'] == 42_000


def test_board_api_exposes_only_service_configuration(client: TestClient):
    saved = client.put(
        '/api/board',
        json={
            'host': 'bench.local',
            'port': 2222,
            'cpu': 3,
        },
    ).json()
    status = client.post('/api/board/test').json()

    assert saved == {
        'host': 'bench.local',
        'port': 2222,
        'cpu': 3,
        'service': 'soyo-benchd',
    }
    assert 'password' not in status


def test_board_configuration_is_passed_to_http_client(tmp_path: Path, monkeypatch):
    calls = []

    class RecordingBoard:
        def __init__(self, config):
            self.config = config

        async def __aenter__(self):
            return self

        async def __aexit__(self, *_args):
            return None

        async def queue(self):
            return []

        async def cancel_job(self, _job_id):
            return None

        async def status(self):
            calls.append(('status', self.config))
            return {
                'online': True,
                'architecture': 'aarch64',
                'cpus': '0-3',
                'frequency_khz': 1_200_000,
                'temperature_millidegrees': 42_000,
                'latency_ms': 3.0,
            }

        async def run_case(self, run):
            calls.append(('run', self.config, run))
            await run.on_running()
            run.artifact_dir.mkdir(parents=True)
            (run.artifact_dir / 'program.s').write_text('.text\n')
            (run.artifact_dir / 'program.elf').write_bytes(b'elf')
            return {
                'status': 'PASS',
                'compile_command': 'compiler -S program.sy',
                'run_command': 'POST http://192.168.2.47:2222/jobs',
                'remote_path': f'/jobs/{run.case_id}',
                'compile_stdout': '',
                'compile_stderr': '',
                'stdout': '',
                'stderr': '',
                'returncode': 0,
                'expected_output': run.expected_output,
                'actual_output': combined_output('', 0),
                'warmup_samples': [],
                'samples': [12.5],
                'artifact_dir': str(run.artifact_dir),
            }

    async def fake_prepare_toolchain(compiler, timeout):
        calls.append(('toolchain', compiler, timeout))
        return 'cargo build', '', ''

    monkeypatch.setattr(server, 'BoardClient', RecordingBoard)
    monkeypatch.setattr(server, 'prepare_toolchain', fake_prepare_toolchain)
    app = server.create_app(tmp_path / 'arm-bench.sqlite3')
    with TestClient(app) as client:
        client.put(
            '/api/board',
            json={
                'host': '192.168.2.47',
                'port': 2222,
                'cpu': 3,
            },
        )
        client.post('/api/board/test')
        task = create_task(client, [CASE_A])
        wait_for_status(client, task['id'], 'COMPLETE')

    assert calls[0][0] == 'status'
    assert calls[1][0] == 'toolchain'
    assert calls[2][0] == 'run'
    assert calls[2][1].host == '192.168.2.47'
    assert calls[2][1].cpu == 3
    assert calls[2][2].source == (server.TESTS / CASE_A).read_text()


def test_task_keeps_configuration_and_case_snapshots(client: TestClient):
    client.put(
        '/api/board',
        json={
            'host': 'bench.local',
            'port': 8766,
            'cpu': 1,
        },
    )
    task = create_task(client, [CASE_A], repeats=3)
    finished = wait_for_status(client, task['id'], 'COMPLETE')

    assert finished['number'] == 1
    assert finished['compiler'] == 'SOYO'
    assert finished['board_host'] == 'bench.local'
    assert finished['cpu'] == 1
    assert finished['cases'][0]['status'] == 'PASS'
    assert finished['cases'][0]['started_at'] is not None
    assert finished['cases'][0]['run_started_at'] >= finished['cases'][0]['started_at']
    case = case_detail(client, task['id'], finished['cases'][0]['id'])
    assert len(case['samples']) == 3
    assert case['median_ms'] == statistics.median(case['samples'])
    assert case['compile_command'].startswith('test-cc SOYO -O2')
    assert case['run_command'].endswith('/jobs')
    assert case['artifacts'] == ['assembly', 'elf', 'raana']

    with server.connect_db(client.app.state.database_path) as connection:
        saved_case = connection.execute('SELECT * FROM task_cases').fetchone()
        source_content = server.get_blob(connection, saved_case['source_blob_hash'])
        expected_content = server.get_blob(connection, saved_case['expected_blob_hash'])
        columns = {row['name'] for row in connection.execute('PRAGMA table_info(tasks)')}
    assert source_content == server.read_case_file(server.TESTS / CASE_A)
    assert expected_content == server.read_case_file((server.TESTS / CASE_A).with_suffix('.out'))
    assert 'password' not in columns


def test_board_result_classes(tmp_path: Path, monkeypatch):
    statuses = ['PASS', 'WA', 'CE', 'RE', 'TLE', 'ERROR']
    install_test_board(monkeypatch, statuses, delay=0)
    app = server.create_app(tmp_path / 'arm-bench.sqlite3')
    with TestClient(app) as client:
        task = create_task(client, [CASE_A, CASE_B, CASE_A, CASE_B, CASE_A, CASE_B])
        finished = wait_for_status(client, task['id'], 'COMPLETE')
        details = [case_detail(client, task['id'], case['id']) for case in finished['cases']]
        expected = case_content(client, task['id'], finished['cases'][0]['id'], 'expected_output')
        actual_pass = case_content(client, task['id'], finished['cases'][0]['id'], 'actual_output')
        actual_wa = case_content(client, task['id'], finished['cases'][1]['id'], 'actual_output')

    assert [case['status'] for case in finished['cases']] == statuses
    assert all(case['compile_command'] for case in details)
    assert actual_pass == expected
    assert actual_wa != expected


def test_tasks_and_cases_run_strictly_serially(client: TestClient):
    first = create_task(client, [CASE_A, CASE_B], repeats=2)
    second = create_task(client, [CASE_A])

    first_running = wait_for_status(client, first['id'], 'RUNNING')
    second_queued = client.get(f'/api/tasks/{second["id"]}').json()
    assert first_running['status'] == 'RUNNING'
    assert second_queued['status'] == 'QUEUED'

    first_done = wait_for_status(client, first['id'], 'COMPLETE')
    second_done = wait_for_status(client, second['id'], 'COMPLETE')
    assert first_done['cases'][1]['started_at'] >= first_done['cases'][0]['finished_at']
    assert second_done['started_at'] >= first_done['finished_at']


def test_board_failure_finishes_task_and_queue_continues(tmp_path: Path, monkeypatch):
    install_test_board(monkeypatch, delay=0, failures=['connection failed'])
    app = server.create_app(tmp_path / 'arm-bench.sqlite3')
    with TestClient(app) as client:
        failed = create_task(client, [CASE_A, CASE_B])
        failed = wait_for_status(client, failed['id'], 'ERROR')
        following = create_task(client, [CASE_A])
        following = wait_for_status(client, following['id'], 'COMPLETE')

    assert [case['status'] for case in failed['cases']] == ['ERROR', 'ERROR']
    assert failed['completed_cases'] == failed['total_cases']
    assert following['cases'][0]['status'] == 'PASS'


def test_waiting_task_can_be_cancelled(client: TestClient):
    first = create_task(client, [CASE_A], repeats=50)
    second = create_task(client, [CASE_A, CASE_B])
    wait_for_status(client, first['id'], 'RUNNING')

    cancelled = client.post(f'/api/tasks/{second["id"]}/cancel').json()
    assert cancelled['status'] == 'CANCELLED'
    assert [case['status'] for case in cancelled['cases']] == ['CANCELLED', 'CANCELLED']
    assert [task['id'] for task in client.get('/api/queue').json()] == [first['id']]


def test_running_case_can_be_cancelled_and_task_continues(client: TestClient):
    task = create_task(client, [CASE_A, CASE_B], repeats=5)
    running = wait_for_status(client, task['id'], 'RUNNING')
    first_case = running['cases'][0]

    client.post(f'/api/tasks/{task["id"]}/cases/{first_case["id"]}/cancel')
    finished = wait_for_status(client, task['id'], 'COMPLETE')

    assert [case['status'] for case in finished['cases']] == ['CANCELLED', 'PASS']


def test_websocket_sends_snapshot_and_replays_events(client: TestClient):
    with client.websocket_connect('/api/ws') as websocket:
        snapshot = websocket.receive_json()
    assert snapshot['seq'] == 0
    assert snapshot['type'] == 'snapshot'
    assert snapshot['payload']['tasks'] == []

    task = create_task(client, [CASE_A])
    with client.websocket_connect('/api/ws?lastSeq=0') as websocket:
        event = websocket.receive_json()
    assert event['seq'] == 1
    assert event['type'] == 'task.created'
    assert event['taskId'] == task['id']


def test_task_retry_replaces_only_failed_cases(tmp_path: Path, monkeypatch):
    install_test_board(monkeypatch, ['PASS', 'WA', 'PASS'], delay=0)
    app = server.create_app(tmp_path / 'arm-bench.sqlite3')
    with TestClient(app) as client:
        original = create_task(client, [CASE_A, CASE_B], repeats=2)
        first_run = wait_for_status(client, original['id'], 'COMPLETE')
        first_finished_at = first_run['cases'][0]['finished_at']

        retried = client.post(f'/api/tasks/{original["id"]}/retry')
        assert retried.status_code == 200
        finished = wait_for_status(client, original['id'], 'COMPLETE')

        tasks = client.get('/api/tasks').json()

    assert retried.json()['id'] == original['id']
    assert retried.json()['number'] == original['number']
    assert [case['status'] for case in finished['cases']] == ['PASS', 'PASS']
    assert finished['cases'][0]['finished_at'] == first_finished_at
    assert len(tasks) == 1


def test_case_retry_replaces_result_and_removes_baseline(tmp_path: Path, monkeypatch):
    install_test_board(monkeypatch, ['PASS', 'WA'], delay=0)
    app = server.create_app(tmp_path / 'arm-bench.sqlite3')
    with TestClient(app) as client:
        task = create_task(client, [CASE_A])
        first_run = wait_for_status(client, task['id'], 'COMPLETE')
        case_id = first_run['cases'][0]['id']
        client.put(f'/api/tasks/{task["id"]}/cases/{case_id}/baseline')
        with server.connect_db(client.app.state.database_path) as connection:
            source_hash = connection.execute(
                'SELECT source_blob_hash FROM task_cases WHERE id = ?', (case_id,)
            ).fetchone()['source_blob_hash']

        retried = client.post(f'/api/tasks/{task["id"]}/cases/{case_id}/retry')
        assert retried.status_code == 200
        finished = wait_for_status(client, task['id'], 'COMPLETE')

        with server.connect_db(client.app.state.database_path) as connection:
            saved = connection.execute(
                'SELECT source_blob_hash FROM task_cases WHERE id = ?', (case_id,)
            ).fetchone()

    assert retried.json()['id'] == task['id']
    assert finished['cases'][0]['id'] == case_id
    assert finished['cases'][0]['status'] == 'WA'
    assert finished['cases'][0]['baseline'] is None
    assert saved['source_blob_hash'] == source_hash


def test_case_and_task_baselines_are_embedded_in_results(client: TestClient):
    task = create_task(client, [CASE_A, CASE_B])
    finished = wait_for_status(client, task['id'], 'COMPLETE')
    first, second = finished['cases']

    client.put(f'/api/tasks/{task["id"]}/cases/{first["id"]}/baseline')
    client.put(f'/api/tasks/{task["id"]}/baseline')
    refreshed = client.get(f'/api/tasks/{task["id"]}').json()

    assert refreshed['cases'][0]['baseline']['id'] == first['id']
    assert refreshed['cases'][1]['baseline']['id'] == second['id']


def test_non_pass_result_cannot_be_a_baseline(tmp_path: Path, monkeypatch):
    install_test_board(monkeypatch, ['WA'], delay=0)
    app = server.create_app(tmp_path / 'arm-bench.sqlite3')
    with TestClient(app) as client:
        task = create_task(client, [CASE_A])
        finished = wait_for_status(client, task['id'], 'COMPLETE')
        response = client.put(
            f'/api/tasks/{task["id"]}/cases/{finished["cases"][0]["id"]}/baseline'
        )

    assert response.status_code == 409


def test_delete_removes_task_artifacts_events_and_baseline(client: TestClient):
    task = create_task(client, [CASE_A])
    finished = wait_for_status(client, task['id'], 'COMPLETE')
    case = finished['cases'][0]
    client.put(f'/api/tasks/{task["id"]}/cases/{case["id"]}/baseline')
    artifact_dir = Path(client.app.state.artifact_root) / task['id']
    assert artifact_dir.is_dir()

    response = client.delete(f'/api/tasks/{task["id"]}')

    assert response.status_code == 204
    assert client.get(f'/api/tasks/{task["id"]}').status_code == 404
    assert not artifact_dir.exists()
    with server.connect_db(client.app.state.database_path) as connection:
        assert connection.execute('SELECT COUNT(*) FROM baselines').fetchone()[0] == 0
        assert (
            connection.execute(
                'SELECT COUNT(*) FROM events WHERE task_id = ?', (task['id'],)
            ).fetchone()[0]
            == 0
        )


def test_case_artifacts_and_persisted_events_are_available(client: TestClient):
    task = create_task(client, [CASE_A])
    finished = wait_for_status(client, task['id'], 'COMPLETE')
    case = finished['cases'][0]

    assembly = client.get(f'/api/tasks/{task["id"]}/cases/{case["id"]}/artifacts/assembly')
    elf = client.get(f'/api/tasks/{task["id"]}/cases/{case["id"]}/artifacts/elf')
    events = client.get(f'/api/tasks/{task["id"]}/events').json()

    assert assembly.status_code == 200
    assert '.global main' in assembly.text
    assert elf.content == b'ARM-BENCH-TEST-ELF\n'
    assert {'task.created', 'case.compiling', 'case.running', 'log.append', 'case.completed'} <= {
        event['type'] for event in events
    }
