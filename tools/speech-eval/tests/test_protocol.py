import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
import wave

sys.path.insert(0, str(Path(__file__).parents[1]))
import benchmark


class CorpusTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.audio = self.root / "synthetic.wav"
        with wave.open(str(self.audio), "wb") as audio:
            audio.setnchannels(1)
            audio.setsampwidth(2)
            audio.setframerate(16000)
            audio.writeframes(bytes(32000))
        self.fixture = {"id": "synthetic-1", "audio": "synthetic.wav",
                        "sha256": hashlib.sha256(self.audio.read_bytes()).hexdigest(),
                        "reference": "C++ uses 20 threads", "terms": ["C++"],
                        "kind": "synthetic", "track": "utterance", "tags": ["technical", "numbers"],
                        "license": "CC0-1.0", "source_url": "https://example.invalid/synthetic",
                        "source_revision": "owned-test-v1"}
        self.path = self.root / "corpus.json"

    def load(self, fixtures=None):
        self.path.write_text(json.dumps({"schema_version": 1, "fixtures": fixtures or [self.fixture]}))
        return benchmark.load_corpus(self.path)

    def test_validated_fixture_has_real_duration_and_provenance(self):
        doc = self.load()
        self.assertEqual(doc["fixtures"][0]["duration_ms"], 1000)
        self.assertEqual(doc["fixtures"][0]["audio_path"], self.audio.resolve())
        self.assertEqual(doc["manifest_sha256"], hashlib.sha256(self.path.read_bytes()).hexdigest())

    def test_corrupt_hash_missing_license_and_duplicate_ids_refuse(self):
        for field, value in [("sha256", "0" * 64), ("license", ""), ("source_revision", ""), ("reference", None)]:
            fixture = dict(self.fixture, **{field: value})
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.load([fixture])
        with self.assertRaises(ValueError):
            self.load([self.fixture, self.fixture])

    def test_escaping_asset_path_and_symlink_refuse(self):
        for path in ["../outside.wav", str(self.audio.resolve())]:
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.load([dict(self.fixture, audio=path)])
        outside = self.root.parent / (self.root.name + "-outside.wav")
        outside.write_bytes(self.audio.read_bytes())
        self.addCleanup(outside.unlink)
        link = self.root / "escape.wav"
        link.symlink_to(outside)
        with self.assertRaises(ValueError):
            self.load([dict(self.fixture, audio=link.name)])

    def test_unsupported_or_truncated_wav_is_not_validated_as_real_audio(self):
        with wave.open(str(self.audio), "wb") as audio:
            audio.setnchannels(2)
            audio.setsampwidth(2)
            audio.setframerate(48000)
            audio.writeframes(bytes(400))
        self.fixture["sha256"] = hashlib.sha256(self.audio.read_bytes()).hexdigest()
        with self.assertRaises(ValueError):
            self.load()
        self.audio.write_bytes(b"not a WAV")
        self.fixture["sha256"] = hashlib.sha256(self.audio.read_bytes()).hexdigest()
        with self.assertRaises(ValueError):
            self.load()


class ResponseTests(unittest.TestCase):
    def parse(self, doc, fmt="asr-eval-v1"):
        return benchmark.parse_response(json.dumps(doc).encode(), fmt, 1000)

    def test_missing_output_is_failure_but_explicit_empty_speech_is_valid(self):
        for text in [None, [], 4]:
            with self.assertRaises(ValueError):
                self.parse({"schema_version": 1, "text": text})
        with self.assertRaises(ValueError):
            self.parse({"schema_version": 1})
        self.assertEqual(self.parse({"schema_version": 1, "text": ""})["text"], "")
        with self.assertRaises(ValueError):
            benchmark.parse_response(b"\xff", "asr-eval-v1", 1000)
        with self.assertRaises(ValueError):
            self.parse({"schema_version": 1, "text": "hello", "anything": float("nan")})

    def test_timestamp_units_and_recording_bounds_are_explicit(self):
        segment = {"text": "hello", "start_ms": 0, "end_ms": 800}
        doc = {"schema_version": 1, "text": "hello", "timestamp_unit": "ms", "segments": [segment]}
        self.assertEqual(self.parse(doc)["segments"], [segment])
        for bad in [dict(doc, timestamp_unit="s"), dict(doc, timestamp_unit=None),
                    dict(doc, segments=[dict(segment, end_ms=1001)]),
                    dict(doc, segments=[dict(segment, start_ms=.1)]),
                    dict(doc, segments=[dict(segment, end_ms=-1)])]:
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                self.parse(bad)

    def test_real_production_smoke_format_is_distinct_from_generic_protocol(self):
        doc = {"legs": 1, "model": "whisper-fixture", "transcripts": ["hello"],
               "audio_seconds_per_leg": 1.0, "load_seconds": .1,
               "inference_seconds": .2, "total_seconds": .3}
        result = self.parse(doc, "fotw-whisper-smoke-v1")
        self.assertEqual(result["text"], "hello")
        self.assertFalse(result["timestamps_supported"])
        self.assertEqual(result["reported_inference_ms"], 200)
        for bad in [dict(doc, legs=2), dict(doc, transcripts=[]), dict(doc, audio_seconds_per_leg=2.0)]:
            with self.assertRaises(ValueError):
                self.parse(bad, "fotw-whisper-smoke-v1")


class JsonTests(unittest.TestCase):
    def test_duplicate_fields_and_overflowing_numbers_refuse_without_ambiguous_policy(self):
        for raw in [b'{"mode":"local","mode":"cloud"}', b'{"x":1e999}',
                    b'{"x":NaN}', ('{"x":' + '9' * 100000 + '}').encode()]:
            with self.subTest(size=len(raw)), self.assertRaises(ValueError):
                benchmark.strict_json(raw)


class DepthTests(unittest.TestCase):
    def test_deeply_nested_adapter_output_is_a_value_error_not_a_run_ending_crash(self):
        with self.assertRaises(ValueError):
            benchmark.parse_response(b"[" * 100000, "asr-eval-v1", 1000)
