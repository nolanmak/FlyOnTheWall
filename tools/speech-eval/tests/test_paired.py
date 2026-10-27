import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
import wave

sys.path.insert(0, str(Path(__file__).parents[1]))
import benchmark
import runner


class PairedTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.audio = self.root / "source.wav"
        with wave.open(str(self.audio), "wb") as audio:
            audio.setnchannels(1)
            audio.setsampwidth(2)
            audio.setframerate(16000)
            audio.writeframes(bytes(32000))
        self.model = self.root / "owned-model.bin"
        self.model.write_bytes(b"synthetic model bytes")
        self.corpus = self.root / "corpus.json"
        self.corpus.write_text(json.dumps({"schema_version": 1, "fixtures": [{
            "id": "utterance-1", "audio": self.audio.name,
            "sha256": benchmark.sha256_file(self.audio),
            "reference": "C++ uses 20 threads", "terms": ["C++"],
            "kind": "synthetic", "track": "utterance", "tags": ["technical", "numbers"],
            "license": "CC0-1.0", "source_url": "https://example.invalid/owned-fixture",
            "source_revision": "owned-test-v1",
        }]}))
        self.specs = self.root / "candidates.json"
        self.calls = []

    def candidate(self, identity="good"):
        executable = Path(sys.executable).resolve()
        return {"id": identity, "mode": "local", "format": "asr-eval-v1",
                "executable": str(executable), "executable_sha256": benchmark.sha256_file(executable),
                "runtime_revision": "owned-adapter-v1", "model_id": identity,
                "model_path": str(self.model), "model_sha256": benchmark.sha256_file(self.model),
                "argv": ["{audio_wav}", "{model_id}"], "timeout_seconds": 2}

    def execute(self, argv, timeout_seconds, **kwargs):
        self.calls.append((argv, kwargs))
        audio = Path(next(a for a in argv if a.endswith(".wav")))
        text = "C++ uses 20 threads" if argv[-1] == "good" else "C++ uses 21 threads"
        return {"status": "ok", "stdout": json.dumps({"schema_version": 1, "text": text,
                "audio_sha256": benchmark.sha256_file(audio), "model_id": argv[-1]}).encode(),
                "elapsed_ms": 12, "measurement": "fresh_process_wall", "returncode": 0,
                "stderr_bytes": 0}

    def run_pair(self, candidates, **kwargs):
        self.specs.write_text(json.dumps({"schema_version": 1, "candidates": candidates}))
        return runner.run(self.corpus, self.specs, executor=kwargs.pop("executor", self.execute),
                          guard=kwargs.pop("guard", lambda argv: (["test-only-guard"] + argv, "test-only")), **kwargs)

    def test_pair_uses_one_snapshot_rotates_order_and_reports_real_scoring_scope(self):
        report = self.run_pair([self.candidate(), self.candidate("poor")], repetitions=2)
        self.assertTrue(report["paired_execution_complete"])
        self.assertEqual(len(self.calls), 4)
        self.assertEqual([a[-1] for a, _ in self.calls], ["good", "poor", "poor", "good"])
        inputs = [next(a for a in args if a.endswith(".wav")) for args, _ in self.calls]
        self.assertEqual(len(set(inputs)), 1)
        self.assertNotEqual(inputs[0], str(self.audio))
        self.assertEqual(report["comparison"]["good"]["all"]["wer"], 0)
        self.assertEqual(report["comparison"]["poor"]["all"]["numeric_errors"], 2)
        self.assertEqual(report["comparison"]["poor"]["subsets"]["synthetic/numbers"]["wer"], .25)
        self.assertEqual(report["candidates"][0]["timing"]["measurement"], "fresh_process_wall")
        self.assertIn("warm_release_to_result", report["unmeasured"])
        self.assertEqual(report["rows"][0]["fresh_process_real_time_factor"], .012)
        self.assertEqual(report["candidates"][0]["timing_by_duration_ms"]["1000.0"]["count"], 2)
        self.assertNotIn("C++ uses", json.dumps(report))

    def test_cloud_without_opt_in_skips_before_credentials_or_execution(self):
        cloud = self.candidate("nova3")
        cloud.update(mode="cloud", credential_env="DEEPGRAM_API_KEY")
        cloud.pop("model_path")
        cloud.pop("model_sha256")
        def forbidden(_name):
            self.fail("credential lookup before explicit cloud opt-in")
        report = self.run_pair([self.candidate(), cloud], credential_reader=forbidden)
        self.assertEqual(len(self.calls), 1)
        self.assertFalse(report["paired_execution_complete"])
        self.assertIsNone(report["comparison"])
        self.assertEqual(report["rows"][1]["status"], "skipped_cloud_not_enabled")

    def test_one_failed_or_wrong_source_response_cannot_be_a_paired_pass(self):
        def wrong(argv, timeout_seconds, **kwargs):
            result = self.execute(argv, timeout_seconds, **kwargs)
            result["stdout"] = json.dumps({"schema_version": 1, "text": "hello",
                                           "audio_sha256": "0" * 64, "model_id": "good"}).encode()
            return result
        report = self.run_pair([self.candidate()], executor=wrong)
        self.assertFalse(report["paired_execution_complete"])
        self.assertEqual(report["rows"][0]["status"], "invalid_response")
        self.assertIsNone(report["comparison"])

    def test_model_pin_mismatch_refuses_before_starting_any_process(self):
        spec = self.candidate()
        self.model.write_bytes(b"changed")
        report = self.run_pair([spec])
        self.assertEqual(self.calls, [])
        self.assertEqual(report["rows"][0]["status"], "asset_mismatch")
        self.assertFalse(report["paired_execution_complete"])

    def test_mutated_snapshot_invalidates_and_stops_the_remaining_pair(self):
        def mutate(argv, timeout_seconds, **kwargs):
            result = self.execute(argv, timeout_seconds, **kwargs)
            Path(next(a for a in argv if a.endswith(".wav"))).write_bytes(b"changed")
            return result
        report = self.run_pair([self.candidate(), self.candidate("poor")], executor=mutate)
        self.assertFalse(report["paired_execution_complete"])
        self.assertEqual(len(self.calls), 1)
        self.assertEqual([r["status"] for r in report["rows"]], ["input_changed", "input_changed"])
        self.assertEqual(benchmark.sha256_file(self.audio), json.loads(self.corpus.read_text())["fixtures"][0]["sha256"])

    def test_duplicate_candidates_and_local_credential_configuration_refuse(self):
        good = self.candidate()
        for specs in [[good, good], [dict(good, credential_env="DEEPGRAM_API_KEY")]]:
            with self.subTest(specs=specs), self.assertRaises(ValueError):
                self.run_pair(specs)
        self.assertEqual(self.calls, [])

    def test_cloud_opt_in_requires_a_key_and_never_reports_credential_values(self):
        cloud = self.candidate("nova3")
        cloud.update(mode="cloud", credential_env="DEEPGRAM_API_KEY")
        cloud.pop("model_path")
        cloud.pop("model_sha256")
        report = self.run_pair([self.candidate(), cloud], allow_cloud=True, credential_reader=lambda _: None)
        self.assertEqual(report["rows"][1]["status"], "skipped_missing_credential")
        self.calls.clear()
        looked_up = []
        def key(name):
            looked_up.append(name)
            return "synthetic-credential-never-print"
        report = self.run_pair([self.candidate(), cloud], allow_cloud=True, credential_reader=key)
        self.assertEqual(looked_up, ["DEEPGRAM_API_KEY"])
        self.assertTrue(report["paired_execution_complete"])
        self.assertNotIn("synthetic-credential-never-print", json.dumps(report))
        self.assertEqual(self.calls[1][1]["extra_env"], {"DEEPGRAM_API_KEY": "synthetic-credential-never-print"})

    def test_unconfigured_and_unsupported_offline_adapters_are_explicit_skips(self):
        report = self.run_pair([self.candidate(), {"id": "parakeet", "mode": "local", "unconfigured": True}])
        self.assertFalse(report["paired_execution_complete"])
        self.assertEqual(report["rows"][1]["status"], "skipped_unconfigured")
        self.calls.clear()
        report = self.run_pair([self.candidate()], guard=lambda _: None)
        self.assertEqual(self.calls, [])
        self.assertEqual(report["rows"][0]["status"], "unsupported_offline_guard")

    def test_directory_models_and_external_runtime_assets_are_fingerprinted(self):
        folder = self.root / "model-bundle"
        folder.mkdir()
        (folder / "weights.bin").write_bytes(b"synthetic weights")
        (folder / "config.json").write_text("{}")
        candidate = dict(self.candidate(), model_path=str(folder), model_sha256=runner.asset_digest(folder))
        script = self.root / "adapter.py"
        script.write_text("# owned fixture adapter")
        candidate["runtime_assets"] = [{"id": "adapter", "path": str(script), "sha256": runner.asset_digest(script)}]
        report = self.run_pair([candidate, self.candidate("poor")])
        self.assertTrue(report["paired_execution_complete"])
        self.calls.clear()
        (folder / "weights.bin").write_bytes(b"changed")
        report = self.run_pair([candidate])
        self.assertEqual(report["rows"][0]["status"], "asset_mismatch")
        self.assertEqual(self.calls, [])
        (folder / "escape").symlink_to(self.model)
        with self.assertRaises(ValueError):
            runner.asset_digest(folder)

    def test_bad_pins_and_budgets_fail_before_execution(self):
        good = self.candidate()
        for bad in [dict(good, executable_sha256=42), dict(good, model_sha256=None),
                    dict(good, timeout_seconds=True), dict(good, use_terms="yes")]:
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                self.run_pair([bad])
        self.assertEqual(self.calls, [])
        with self.assertRaises(ValueError):
            self.run_pair([good], repetitions=0)
        with self.assertRaises(ValueError):
            self.run_pair([good], max_total_seconds=0)

    def test_scoring_limits_are_not_reported_as_protocol_failure_or_a_complete_pair(self):
        corpus = json.loads(self.corpus.read_text())
        corpus["fixtures"][0].update(reference="word " * 4000, terms=[])
        self.corpus.write_text(json.dumps(corpus))
        def dense(argv, timeout_seconds, **kwargs):
            result = self.execute(argv, timeout_seconds, **kwargs)
            value = json.loads(result["stdout"])
            value["text"] = "word " * 4000
            result["stdout"] = json.dumps(value).encode()
            return result
        report = self.run_pair([self.candidate(), self.candidate("poor")], executor=dense)
        self.assertFalse(report["paired_execution_complete"])
        self.assertEqual([r["status"] for r in report["rows"]], ["scoring_work_limit"] * 2)
        self.assertIsNone(report["comparison"])

    def test_real_owned_generic_cli_executes_both_pinned_adapters_and_discards_text(self):
        script = self.root / "owned adapter.py"
        script.write_text('import hashlib,json,sys\nfrom pathlib import Path\np=Path(sys.argv[1])\nprint(json.dumps({"schema_version":1,"text":"C++ uses 20 threads","audio_sha256":hashlib.sha256(p.read_bytes()).hexdigest(),"model_id":sys.argv[2]}))\n')
        candidates = []
        for identity in ("good", "poor"):
            candidate = self.candidate(identity)
            candidate["argv"] = [str(script), "{audio_wav}", "{model_id}"]
            candidate["runtime_assets"] = [{"id": "owned-script", "path": str(script), "sha256": runner.asset_digest(script)}]
            candidates.append(candidate)
        report = self.run_pair(candidates, executor=benchmark.owned_command,
                               guard=lambda argv: (argv, "owned-model-free-test"))
        self.assertTrue(report["paired_execution_complete"])
        self.assertEqual([r["status"] for r in report["rows"]], ["ok", "ok"])
        self.assertEqual(report["comparison"]["good"]["all"]["wer"], 0)
        self.assertNotIn("C++ uses 20 threads", json.dumps(report))
        script.write_text("# runtime changed after pinning")
        report = self.run_pair(candidates)
        self.assertFalse(report["paired_execution_complete"])
        self.assertEqual([r["status"] for r in report["rows"]], ["asset_mismatch"] * 2)
