#!/usr/bin/env python3
"""Opt-in macOS comparison of owned FOTW/Murmur inference processes.

Uses existing hash-pinned public assets only. No capture, insertion, credentials,
downloads or installed-app launch. Results describe adapters, not full apps.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import re
import select
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import wave

MODEL_SHA = 'c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d'
WAV_SHA = '59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e'
REFERENCE_SHA = '880038ec1c8c450de59e56c613f14ac01b455a934e4752321e07f1a7cbcec0ee'


def sha(path):
    """Hash identity without loading the model into Python memory."""
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def reference_matches(text):
    return hashlib.sha256(' '.join(re.findall('[a-z0-9]+', text.lower())).encode()).hexdigest() == REFERENCE_SHA


@contextmanager
def owned_directory():
    """Retain private control state if authenticated runtime shutdown fails."""
    path = Path(tempfile.mkdtemp(prefix='fotw-murmur-contention-'))
    try:
        yield path
    finally:
        if (path / 'state' / 'runtime.json').exists():
            raise RuntimeError(f'Owned runtime may remain active; retained state: {path}')
        shutil.rmtree(path)


class Memory:
    """Sample only explicitly registered owned PIDs; RSS is not unique memory."""
    def __init__(self, roles):
        self.roles = dict(roles)
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.rows = []
        self.error = None
        self.thread = threading.Thread(target=self.sample)

    def sample(self):
        try:
            while not self.stop.is_set():
                with self.lock:
                    roles = dict(self.roles)
                result = subprocess.run(
                    ['/bin/ps', '-p', ','.join(map(str, roles)), '-o', 'pid=,rss='],
                    capture_output=True, text=True, timeout=5)
                values = {}
                for line in result.stdout.splitlines():
                    pid, kib = map(int, line.split())
                    if pid in roles:
                        values[roles[pid]] = kib * 1024
                self.rows.append(values)
                self.stop.wait(.05)
        except Exception as error:
            self.error = type(error).__name__
            self.stop.set()

    def register(self, role, pid):
        with self.lock:
            self.roles[pid] = role

    def finish(self):
        self.stop.set()
        self.thread.join(timeout=6)
        assert not self.thread.is_alive()
        assert self.error is None, 'Owned memory sampler failed: ' + str(self.error)
        assert self.rows, 'No owned-process memory samples'
        roles = set().union(*(row.keys() for row in self.rows))
        return {
            'sample_count': len(self.rows),
            'per_role_peak_rss_bytes': {role: max(row.get(role, 0) for row in self.rows) for role in roles},
            'peak_sampled_sum_rss_bytes': max(sum(row.values()) for row in self.rows),
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ['model', 'wav', 'fotw', 'murmur', 'murmur-source', 'server', 'output']:
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    for name, value in vars(args).items():
        setattr(args, name, value.resolve())
    assert args.model.stat().st_size == 487614201 and sha(args.model) == MODEL_SHA
    assert args.wav.stat().st_size == 352078 and sha(args.wav) == WAV_SHA
    assert args.output.parent.is_dir()
    identities = {name: sha(getattr(args, name)) for name in ['fotw', 'murmur', 'server']}
    scripts = args.murmur_source / 'scripts'
    sandbox = ['/usr/bin/sandbox-exec', '-f', str(scripts / 'local-whisper-offline.sb')]

    def command(argv, timeout=90):
        result = subprocess.run(sandbox + list(map(str, argv)), capture_output=True, timeout=timeout)
        assert result.returncode == 0, 'Owned helper/client command failed'
        return result

    with owned_directory() as directory:
        with wave.open(str(args.wav)) as source:
            assert (source.getnchannels(), source.getsampwidth(), source.getframerate(), source.getnframes()) == (1, 2, 16000, 176000)
            pcm = source.readframes(176000)
        f32 = directory / 'public.f32le'
        f32.write_bytes(b''.join(struct.pack('<f', v[0] / 32768) for v in struct.iter_unpack('<h', pcm)))
        helper = [sys.executable, scripts / 'local-whisper.py', '--app', 'murmur', '--state-dir', directory / 'state']
        trials = []
        try:
            command(helper + ['import-model', args.model, '--sha256', MODEL_SHA])
            with socket.socket() as probe:
                probe.bind(('127.0.0.1', 0))
                port = probe.getsockname()[1]
            ready = json.loads(command(helper + ['start', '--binary', args.server, '--port', port, '--timeout', 60]).stdout)
            assert ready['ready'] and ready['murmur_capabilities']['model_sha256'] == MODEL_SHA
            assert ready['url'] == f'http://127.0.0.1:{port}'
            pids = [int(line[1:]) for line in subprocess.check_output(['/usr/sbin/lsof', '-nP', f'-iTCP:{port}', '-sTCP:LISTEN', '-Fp'], text=True, timeout=10).splitlines() if line.startswith('p')]
            assert len(pids) == 1
            gateway = pids[0]
            children = list(map(int, subprocess.check_output(['/usr/bin/pgrep', '-P', str(gateway)], text=True, timeout=10).split()))
            assert len(children) == 1
            engine = children[0]
            assert Path(subprocess.check_output(['/bin/ps', '-p', str(engine), '-o', 'comm='], text=True, timeout=10).strip()).resolve() == args.server

            # Rotate order; the managed Murmur runtime remains loaded throughout.
            modes = ['murmur-only', 'fotw-idle', 'fotw-only', 'concurrent']
            for repeat in range(3):
                for mode in modes[repeat:] + modes[:repeat]:
                    memory = Memory({gateway: 'gateway', engine: 'murmur-engine'})
                    fotw = None
                    memory.thread.start()
                    try:
                        if mode != 'murmur-only':
                            fotw = subprocess.Popen(sandbox + [str(args.fotw), str(args.model), str(f32), '2', '--wait-for-start'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
                            memory.register('fotw-engine', fotw.pid)
                            assert select.select([fotw.stdout], [], [], 30)[0], 'FOTW readiness timeout'
                            assert json.loads(fotw.stdout.readline()) == {'ready': True, 'legs': 2}
                        time.sleep(.1)
                        result = {'repeat': repeat + 1, 'mode': mode}

                        def murmur_request():
                            started = time.monotonic()
                            client = subprocess.Popen(sandbox + [str(args.murmur), ready['url'], str(args.wav), 'en'], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
                            memory.register('murmur-client', client.pid)
                            try:
                                output, _ = client.communicate(timeout=60)
                                assert client.returncode == 0 and reference_matches(output.decode())
                                return {'wall_seconds': time.monotonic() - started, 'reference_matches': True, 'started_monotonic': started}
                            finally:
                                if client.poll() is None:
                                    client.kill()
                                    client.wait()

                        with ThreadPoolExecutor(max_workers=1) as executor:
                            future = executor.submit(murmur_request) if mode != 'fotw-only' else None
                            if mode in ['fotw-only', 'concurrent']:
                                gate_time = time.monotonic()
                                output, _ = fotw.communicate(input=b'1', timeout=60)
                                ended = time.monotonic()
                                native = json.loads(output)
                                assert fotw.returncode == 0 and len(native['transcripts']) == 2
                                assert all(reference_matches(text) for text in native['transcripts'])
                                result['fotw'] = {'inference_seconds': native['inference_seconds'], 'reference_matches': [True, True]}
                            if future:
                                result['murmur'] = future.result(timeout=65)
                                started = result['murmur'].pop('started_monotonic')
                                if mode == 'concurrent':
                                    result['request_intervals_overlap'] = max(started, gate_time) < min(started + result['murmur']['wall_seconds'], ended)
                                    assert result['request_intervals_overlap']
                                    result['dispatch_skew_seconds'] = started - gate_time
                        if mode == 'fotw-idle':
                            output, _ = fotw.communicate(input=b'', timeout=10)
                            assert fotw.returncode != 0 and not output
                        result['memory'] = memory.finish()
                        expected = {'gateway', 'murmur-engine'}
                        if fotw:
                            expected.add('fotw-engine')
                        if mode != 'fotw-only':
                            expected.add('murmur-client')
                        assert expected <= result['memory']['per_role_peak_rss_bytes'].keys(), 'Missing expected owned-process memory samples'
                        trials.append(result)
                        print(mode, repeat + 1, 'reference matched', flush=True)
                    finally:
                        if not memory.stop.is_set():
                            memory.finish()
                        if fotw and fotw.poll() is None:
                            fotw.kill()
                            fotw.wait()
        finally:
            command(helper + ['stop'])
            assert not (directory / 'state' / 'runtime.json').exists()
    assert identities == {name: sha(getattr(args, name)) for name in identities}, 'Executed binary changed during trials'
    root = Path(__file__).resolve().parents[1]
    report = {
        'source_commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True, timeout=10).strip(),
        'tracked_source_dirty': bool(subprocess.check_output(['git', 'diff', 'HEAD', '--name-only'], cwd=root, text=True, timeout=10).strip()),
        'binary_sha256': identities, 'model_sha256': MODEL_SHA, 'wave_sha256': WAV_SHA,
        'murmur_helper_sha256': {name: sha(scripts / name) for name in ['local-whisper.py', 'whisper_gateway.py', 'local-whisper-offline.sb']},
        'hardware_model': subprocess.check_output(['/usr/sbin/sysctl', '-n', 'hw.model'], text=True, timeout=10).strip(),
        'ram_bytes': int(subprocess.check_output(['/usr/sbin/sysctl', '-n', 'hw.memsize'], text=True, timeout=10)),
        'macos': subprocess.check_output(['/usr/bin/sw_vers', '-productVersion'], text=True, timeout=10).strip(),
        'audio_seconds_per_recognizer': 11, 'fotw_recognizers': 2, 'repetitions': 3,
        'rss_sampling_interval_seconds': .05, 'rss_is_not_unique_physical_memory': True,
        'engine_defaults': 'FOTW release whisper-rs Metal; supplied Murmur server defaults, automatic GPU selection',
        'input_pacing': 'accelerated whole public clip per recognizer, not realtime recording',
        'cache_controlled': False, 'murmur_startup_memory_measured': False, 'fotw_loading_memory_sampled': True, 'gpu_memory_separately_measured': False,
        'external_network_denied': True, 'owned_runtime_stopped': True,
        'temporary_state_removed': not directory.exists(), 'transcripts_retained': False,
        'full_app_or_device_acceptance': False, 'shared_worker_evaluated': False,
        'trials': trials,
    }
    args.output.write_text(json.dumps(report, indent=2) + '\n')


if __name__ == '__main__':
    main()
