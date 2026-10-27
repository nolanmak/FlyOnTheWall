import contextlib
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit
import wave

sys.path.insert(0, str(Path(__file__).parents[1]))
import deepgram_adapter as adapter
import benchmark
import runner


class DeepgramTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.audio = Path(self.tmp.name) / 'owned.wav'
        with wave.open(str(self.audio), 'wb') as audio:
            audio.setnchannels(1)
            audio.setsampwidth(2)
            audio.setframerate(16000)
            audio.writeframes(bytes(32000))
        self.model = 'nova-3@2026-01-01.12345'
        self.reply = {'metadata': {'models': ['owned-model'], 'model_info': {
            'owned-model': {'arch': 'nova-3', 'version': '2026-01-01.12345'}}},
            'results': {'channels': [{'alternatives': [{'transcript': 'C++ uses 20 threads'}]}]}}
        self.status = 200
        self.raw = None
        self.requests = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers['Content-Length']))
                owner.requests.append((self.path, dict(self.headers), body))
                self.send_response(owner.status)
                self.send_header('Location', 'https://example.invalid/never-follow')
                self.end_headers()
                try:
                    self.wfile.write(owner.raw if owner.raw is not None else json.dumps(owner.reply).encode())
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def log_message(self, *_args):
                pass

        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(thread.join)
        self.addCleanup(server.shutdown)
        self.port = server.server_port

    def connection(self, hostname, **kwargs):
        self.assertEqual(hostname, 'api.deepgram.com')
        self.assertGreater(kwargs['timeout'], 0)
        return http.client.HTTPConnection('127.0.0.1', self.port, timeout=2)

    def transcribe(self, terms=None, **kwargs):
        with patch.object(adapter.http.client, 'HTTPSConnection', self.connection):
            return adapter.transcribe(self.audio, self.model, terms or [], 'owned-test-value', **kwargs)

    def test_real_loopback_receives_exact_wav_and_raw_versioned_request(self):
        result = self.transcribe(['C++', 'customer service', 'x&model=wrong'])
        path, headers, body = self.requests[0]
        query = parse_qs(urlsplit(path).query)
        self.assertEqual(urlsplit(path).path, '/v1/listen')
        self.assertEqual(query['keyterm'], ['C++', 'customer service', 'x&model=wrong'])
        self.assertEqual(query['model'], ['nova-3'])
        self.assertEqual(query['version'], ['2026-01-01.12345'])
        for key in ('smart_format', 'punctuate', 'diarize', 'utterances'):
            self.assertEqual(query[key], ['false'])
        self.assertEqual(query['mip_opt_out'], ['true'])
        self.assertEqual(query['language'], ['en'])
        self.assertEqual(headers['Authorization'], 'Token owned-test-value')
        self.assertEqual(headers['Content-Type'], 'audio/wav')
        self.assertEqual(body, self.audio.read_bytes())
        self.assertEqual(result, {'schema_version': 1, 'audio_sha256': hashlib.sha256(body).hexdigest(),
                                 'model_id': self.model, 'text': 'C++ uses 20 threads'})

    def test_empty_transcript_is_valid_but_missing_transcript_is_not_silence(self):
        self.reply['results']['channels'][0]['alternatives'][0]['transcript'] = ''
        self.assertEqual(self.transcribe()['text'], '')
        del self.reply['results']['channels'][0]['alternatives'][0]['transcript']
        with self.assertRaises(ValueError):
            self.transcribe()

    def test_wrong_or_missing_server_model_receipt_is_rejected(self):
        for info in ({'arch': 'nova-2', 'version': '2026-01-01.12345'},
                     {'arch': 'nova-3', 'version': '2026-01-02.99999'}, {}, None):
            with self.subTest(info=info):
                self.reply['metadata']['model_info']['owned-model'] = info
                with self.assertRaises(ValueError):
                    self.transcribe()

    def test_http_errors_redirects_and_malformed_or_large_responses_are_failures(self):
        for status in (202, 302, 401, 429, 500):
            with self.subTest(status=status):
                self.status = status
                before = len(self.requests)
                with self.assertRaises(ValueError):
                    self.transcribe()
                self.assertEqual(len(self.requests), before + 1, 'must not redirect or retry uploads')
        self.status = 200
        for raw in (b'not JSON', b'[' * 100000, b'{}', b'x' * 1048577):
            with self.subTest(size=len(raw)):
                self.raw = raw
                with self.assertRaises(ValueError):
                    self.transcribe()

    def test_bad_configuration_is_rejected_before_any_connection(self):
        with patch.object(adapter.http.client, 'HTTPSConnection') as connection:
            for model in ('nova-3', 'nova-3@latest', 'nova-2@2026-01-01.12345',
                          'nova-3@2026-99-99.1'):
                with self.subTest(model=model), self.assertRaises(ValueError):
                    adapter.transcribe(self.audio, model, [], 'owned-test-value')
            for terms in ('not a list', ['x'] * 101, [''], ['bad\nterm']):
                with self.subTest(terms=terms), self.assertRaises(ValueError):
                    adapter.transcribe(self.audio, self.model, terms, 'owned-test-value')
            for credential in ('', 'bad\r\nheader'):
                with self.assertRaises(ValueError):
                    adapter.transcribe(self.audio, self.model, [], credential)
            connection.assert_not_called()

    def test_file_changed_at_read_cannot_upload_bytes_that_were_never_validated(self):
        original_open = Path.open
        def changed_at_read(path, *args, **kwargs):
            if path == self.audio and args == ('rb',):
                with original_open(path, 'wb') as destination:
                    destination.write(b'not a WAV: changed after path validation')
            return original_open(path, *args, **kwargs)
        with patch.object(Path, 'open', changed_at_read):
            with patch.object(adapter.http.client, 'HTTPSConnection') as connection:
                with self.assertRaises(ValueError):
                    adapter.transcribe(self.audio, self.model, [], 'owned-test-value')
                connection.assert_not_called()

    def test_cli_requires_opt_in_before_credential_lookup_and_scrubs_errors(self):
        args = ['--audio', str(self.audio), '--model-id', self.model, '--terms-json', '[]']
        def no_credential(name, default=None):
            if name == 'DEEPGRAM_API_KEY':
                self.fail('credential read before opt-in')
            return default
        with patch.object(adapter.os.environ, 'get', side_effect=no_credential):
            with contextlib.redirect_stderr(io.StringIO()) as error:
                self.assertEqual(adapter.main(args), 2)
            self.assertEqual(error.getvalue(), 'deepgram evaluation failed: cloud opt-in required\n')
        def credential(name, default=None):
            return 'owned-test-value' if name == 'DEEPGRAM_API_KEY' else default
        with patch.object(adapter.os.environ, 'get', side_effect=credential):
            with patch.object(adapter.http.client, 'HTTPSConnection', self.connection):
                self.status = 401
                self.raw = b'owned-test-value private transcript do not print'
                with contextlib.redirect_stdout(io.StringIO()) as output, contextlib.redirect_stderr(io.StringIO()) as error:
                    self.assertEqual(adapter.main(['--allow-cloud'] + args), 2)
                self.assertEqual(output.getvalue(), '')
                self.assertEqual(error.getvalue(), 'deepgram evaluation failed\n')

    def test_real_runner_children_compare_baseline_and_terms_without_provider_access(self):
        root = self.audio.parent
        corpus = root / 'corpus.json'
        corpus.write_text(json.dumps({'schema_version': 1, 'fixtures': [{
            'id': 'owned-synthetic', 'audio': self.audio.name,
            'sha256': benchmark.sha256_file(self.audio), 'reference': 'C++ uses 20 threads',
            'terms': ['C++'], 'kind': 'synthetic', 'track': 'utterance', 'tags': ['technical'],
            'license': 'CC0-1.0', 'source_url': 'https://example.invalid/owned',
            'source_revision': 'owned-test-v1'}]}))
        # The production CLI has no endpoint override. Only this owned fixture
        # patches its connection constructor to our synthetic loopback server.
        shim = root / 'owned_loopback.py'
        shim.write_text('import http.client,sys\n'
                        'sys.path.insert(0,' + repr(str(Path(adapter.__file__).parent)) + ')\n'
                        'import deepgram_adapter as a\n'
                        'def connection(hostname, **kwargs):\n'
                        ' assert hostname == "api.deepgram.com"\n'
                        ' return http.client.HTTPConnection("127.0.0.1",' + str(self.port) + ',timeout=2)\n'
                        'a.http.client.HTTPSConnection=connection\n'
                        'raise SystemExit(a.main())\n')
        executable = Path(sys.executable).resolve()
        assets = [shim, Path(adapter.__file__), Path(benchmark.__file__),
                  Path(adapter.__file__).with_name('speech_eval.py')]
        candidates = []
        for use_terms in (False, True):
            candidates.append({'id': 'terms' if use_terms else 'baseline', 'mode': 'cloud',
                'format': 'asr-eval-v1', 'credential_env': 'DEEPGRAM_API_KEY',
                'executable': str(executable), 'executable_sha256': benchmark.sha256_file(executable),
                'runtime_revision': 'owned-loopback-fixture', 'model_id': self.model,
                'use_terms': use_terms, 'timeout_seconds': 5,
                'runtime_assets': [{'id': 'asset-' + str(i), 'path': str(p),
                                    'sha256': benchmark.sha256_file(p)} for i, p in enumerate(assets)],
                'argv': [str(shim), '--allow-cloud', '--audio', '{audio_wav}',
                         '--model-id', '{model_id}', '--terms-json', '{terms_json}']})
        manifest = root / 'candidates.json'
        manifest.write_text(json.dumps({'schema_version': 1, 'candidates': candidates}))
        def never_read(_name):
            self.fail('credential read in offline mode')
        skipped = runner.run(corpus, manifest, credential_reader=never_read)
        self.assertFalse(skipped['paired_execution_complete'])
        self.assertEqual(self.requests, [])
        report = runner.run(corpus, manifest, allow_cloud=True,
                            credential_reader=lambda _: 'owned-test-value')
        self.assertTrue(report['paired_execution_complete'])
        self.assertEqual([r['status'] for r in report['rows']], ['ok', 'ok'])
        self.assertEqual(report['comparison']['terms']['all']['term_recall'], 1)
        queries = [parse_qs(urlsplit(path).query) for path, _, _ in self.requests]
        self.assertNotIn('keyterm', queries[0])
        self.assertEqual(queries[1]['keyterm'], ['C++'])
        self.assertEqual(self.requests[0][2], self.requests[1][2])
        self.assertNotIn('owned-test-value', json.dumps(report))
        self.assertNotIn('C++ uses', json.dumps(report))


if __name__ == '__main__':
    unittest.main()
