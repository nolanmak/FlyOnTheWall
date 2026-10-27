import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1]))
import run_benchmark
import runner


class CliTests(unittest.TestCase):
    def test_local_command_cannot_enable_cloud_and_cloud_is_a_separate_opt_in(self):
        with tempfile.TemporaryDirectory() as folder:
            for command, enabled in [("run", False), ("run-cloud", True)]:
                output = Path(folder) / (command + ".json")
                report = {"paired_execution_complete": False, "rows": [{"status": "skipped_unconfigured"}]}
                with patch.object(runner, "run", return_value=report) as call, contextlib.redirect_stdout(io.StringIO()):
                    status = run_benchmark.main([command, "--corpus", "corpus.json", "--candidates", "models.json", "--output", str(output)])
                self.assertEqual(status, 1, "incomplete pairing became a successful command")
                self.assertEqual(call.call_args.kwargs["allow_cloud"], enabled)
                self.assertEqual(json.loads(output.read_text()), report)

    def test_existing_report_is_untouched_and_refused_before_running_adapters(self):
        with tempfile.TemporaryDirectory() as folder:
            output = Path(folder) / "report.json"
            output.write_text("previous evidence")
            with patch.object(runner, "run") as call, contextlib.redirect_stderr(io.StringIO()):
                code = run_benchmark.main(["run", "--corpus", "c", "--candidates", "m", "--output", str(output)])
            self.assertEqual(code, 2)
            call.assert_not_called()
            self.assertEqual(output.read_text(), "previous evidence")
            with self.assertRaises(FileExistsError):
                run_benchmark.write_report(output, {})
            self.assertEqual(output.read_text(), "previous evidence")
            self.assertEqual(list(Path(folder).glob(".speech-eval-*")), [])

    def test_coverage_counts_fixtures_once_and_does_not_claim_human_or_license_verification(self):
        def fixture(identity, kind, track, duration, meeting=None):
            return {"id": identity, "kind": kind, "track": track, "duration_ms": duration, "meeting_id": meeting}
        report = runner.corpus_coverage([
            fixture("short", "synthetic", "utterance", 1000),
            fixture("mic", "human", "mic", 600000, "meeting"),
            fixture("system", "human", "system", 600000, "meeting"),
            fixture("other", "human", "mic", 600000, "incomplete"),
        ])
        self.assertTrue(report["annotation_claims_only"])
        self.assertEqual(report["short_utterances_by_kind"], {"human": 0, "synthetic": 1, "silence": 0})
        self.assertEqual(report["ten_minute_paired_meeting_ids"], ["meeting"])
