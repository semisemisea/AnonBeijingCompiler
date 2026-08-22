from __future__ import annotations

import asyncio
import base64
import os
import shlex
import statistics
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Self

import httpx2

ROOT = Path(__file__).resolve().parents[3]
WEB_ROOT = Path(__file__).resolve().parents[1]
AARCH64_SYSROOT = Path(
    os.environ.get('AARCH64_SYSROOT', str(WEB_ROOT / 'data/toolchain/aarch64-linux-gnu'))
)
AARCH64_GCC_TOOLCHAIN = Path(
    os.environ.get('AARCH64_GCC_TOOLCHAIN', str(AARCH64_SYSROOT.parent / 'usr'))
)
POLL_INTERVAL_SECONDS = 0.05


@dataclass(frozen=True)
class BoardConfig:
    host: str
    port: int
    cpu: int


@dataclass(frozen=True)
class CaseRun:
    task_id: str
    case_id: str
    compiler: str
    opt_level: int
    warmups: int
    repeats: int
    timeout_seconds: int
    cpu: int
    source: str
    input_data: str | None
    expected_output: str | None
    artifact_dir: Path
    on_running: Callable[[], Awaitable[None]]


def combined_output(stdout: str, returncode: int) -> str:
    if stdout and not stdout.endswith('\n'):
        stdout += '\n'
    return f'{stdout}{returncode}\n'


async def run_local(command: list[str], timeout: int) -> tuple[int, str, str]:
    process = await asyncio.create_subprocess_exec(
        *command,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        stdout, stderr = await asyncio.wait_for(process.communicate(), timeout)
    except (TimeoutError, asyncio.CancelledError):
        process.kill()
        await process.wait()
        raise
    return process.returncode, stdout.decode(errors='replace'), stderr.decode(errors='replace')


async def prepare_toolchain(compiler: str, timeout: int) -> tuple[str, str, str]:
    command = (
        ['cargo', 'build', '-p', 'soyo_compiler', '--bin', 'compiler']
        if compiler == 'SOYO'
        else ['clang', '--version']
    )
    returncode, stdout, stderr = await run_local(command, timeout)
    if returncode:
        raise OSError(f'toolchain preparation exited with {returncode}: {stderr.strip()}')
    return shlex.join(command), stdout, stderr


class BoardClient:
    def __init__(self, config: BoardConfig):
        self.config = config
        self.client = httpx2.AsyncClient(
            base_url=f'http://{config.host}:{config.port}',
            timeout=httpx2.Timeout(10, read=None),
        )

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *_args: object) -> None:
        await self.client.aclose()

    async def status(self) -> dict[str, Any]:
        started = time.perf_counter()
        response = await self.client.get('/status')
        response.raise_for_status()
        return {**response.json(), 'latency_ms': round((time.perf_counter() - started) * 1000, 2)}

    async def queue(self) -> list[dict[str, Any]]:
        response = await self.client.get('/queue')
        response.raise_for_status()
        return response.json()

    async def cancel_job(self, job_id: str) -> None:
        response = await self.client.delete(f'/jobs/{job_id}')
        response.raise_for_status()

    async def run_case(self, run: CaseRun) -> dict[str, Any]:
        run.artifact_dir.mkdir(parents=True, exist_ok=True)
        source = run.artifact_dir / 'program.sy'
        assembly = run.artifact_dir / 'program.s'
        elf = run.artifact_dir / 'program.elf'
        raana = run.artifact_dir / 'program.raana'
        source.write_bytes(run.source.encode())

        compiler = os.environ.get('SOYO_COMPILER', str(ROOT / 'target/debug/compiler'))
        extra_compile_command = ''
        if run.compiler == 'SOYO':
            compile_command = [
                compiler,
                f'-O{run.opt_level}',
                '--target',
                'aarch64',
                '-S',
                '-o',
                str(assembly),
                str(source),
            ]
            returncode, compile_stdout, compile_stderr = await run_local(
                compile_command, run.timeout_seconds
            )
            if returncode == 0:
                ir_command = [
                    compiler,
                    f'-O{run.opt_level}',
                    '--target',
                    'aarch64',
                    '--emit',
                    'ir',
                    '-o',
                    str(raana),
                    str(source),
                ]
                returncode, ir_stdout, ir_stderr = await run_local(ir_command, run.timeout_seconds)
                compile_stdout += ir_stdout
                compile_stderr += ir_stderr
                extra_compile_command = f'\n{shlex.join(ir_command)}'
        else:
            compile_command = [
                'clang',
                '-x',
                'c',
                '-fcommon',
                '-ffp-contract=off',
                '-Wno-incompatible-pointer-types',
                f'-O{run.opt_level}',
                '--target=aarch64-linux-gnu',
                f'--sysroot={AARCH64_SYSROOT}',
                '-include',
                str(ROOT / 'sysylib/sylib.h'),
                '-S',
                '-o',
                str(assembly),
                str(source),
            ]
            returncode, compile_stdout, compile_stderr = await run_local(
                compile_command, run.timeout_seconds
            )

        compile_command_text = shlex.join(compile_command) + extra_compile_command
        if returncode:
            return {
                'status': 'CE',
                'compile_command': compile_command_text,
                'compile_stdout': compile_stdout,
                'compile_stderr': compile_stderr,
                'samples': [],
                'warmup_samples': [],
                'error': f'compiler exited with {returncode}',
            }

        link_command = [
            'clang',
            '--target=aarch64-linux-gnu',
            f'--gcc-toolchain={AARCH64_GCC_TOOLCHAIN}',
            f'--sysroot={AARCH64_SYSROOT}',
            '-fuse-ld=lld',
            '-static',
            str(assembly),
            str(ROOT / 'sysylib/libsysy_arm.a'),
            '-o',
            str(elf),
        ]
        link_returncode, link_stdout, link_stderr = await run_local(
            link_command, run.timeout_seconds
        )
        compile_stdout += link_stdout
        compile_stderr += link_stderr
        compile_command_text += f'\n{shlex.join(link_command)}'
        if link_returncode:
            return {
                'status': 'CE',
                'compile_command': compile_command_text,
                'compile_stdout': compile_stdout,
                'compile_stderr': compile_stderr,
                'samples': [],
                'warmup_samples': [],
                'error': f'linker exited with {link_returncode}',
            }

        payload = {
            'id': run.case_id,
            'cpu': run.cpu,
            'timeout': run.timeout_seconds,
            'warmups': run.warmups,
            'repeats': run.repeats,
            'elf': base64.b64encode(elf.read_bytes()).decode(),
            'input': base64.b64encode(run.input_data.encode()).decode()
            if run.input_data is not None
            else None,
        }
        response = await self.client.post('/jobs', json=payload)
        response.raise_for_status()
        running_reported = False
        try:
            while True:
                response = await self.client.get(f'/jobs/{run.case_id}')
                response.raise_for_status()
                job = response.json()
                if not running_reported and job['status'] in {'RUNNING', 'COMPLETE'}:
                    await run.on_running()
                    running_reported = True
                if job['status'] in {'COMPLETE', 'ERROR', 'CANCELLED'}:
                    break
                await asyncio.sleep(POLL_INTERVAL_SECONDS)
        except asyncio.CancelledError:
            await self.cancel_job(run.case_id)
            raise

        if job['status'] == 'ERROR':
            raise OSError(job.get('result', {}).get('error', 'board job failed'))
        if job['status'] == 'CANCELLED':
            raise asyncio.CancelledError
        result = job['result']
        await self.cancel_job(run.case_id)
        samples = result['samples']
        warmup_samples = result['warmup_samples']
        actual_output = combined_output(result['stdout'], result['returncode'])
        status = (
            'TLE'
            if result['timed_out']
            else 'PASS'
            if run.expected_output is None or actual_output == run.expected_output
            else 'WA'
        )
        return {
            'status': status,
            'compile_command': compile_command_text,
            'run_command': f'POST http://{self.config.host}:{self.config.port}/jobs',
            'remote_path': f'/jobs/{run.case_id}',
            'compile_stdout': compile_stdout,
            'compile_stderr': compile_stderr,
            'stdout': result['stdout'],
            'stderr': result['stderr'],
            'returncode': result['returncode'],
            'expected_output': run.expected_output,
            'actual_output': actual_output,
            'warmup_samples': warmup_samples,
            'samples': samples,
            'median_ms': statistics.median(samples) if samples else None,
            'min_ms': min(samples) if samples else None,
            'max_ms': max(samples) if samples else None,
            'error': f'timeout after {run.timeout_seconds}s' if result['timed_out'] else None,
            'artifact_dir': str(run.artifact_dir),
        }
