import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1]))
import benchmark


@unittest.skipUnless(os.name == "posix", "owned process-group execution targets macOS/Linux")
class ExecutionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.script = self.root / "owned adapter.py"

    def run_script(self, code, timeout=2, maximum=benchmark.MAX_RESPONSE_BYTES, args=()):
        self.script.write_text(code)
        return benchmark.owned_command([sys.executable, str(self.script), *args], timeout, maximum)

    def test_argument_boundaries_and_no_inherited_provider_credentials(self):
        code = 'import json,os,sys;print(json.dumps({"args":sys.argv[1:],"credential_present":"DEEPGRAM_API_KEY" in os.environ}))'
        with patch.dict(os.environ, {"DEEPGRAM_API_KEY": "synthetic-secret-never-inherit"}):
            result = self.run_script(code, args=("literal $(touch unexpected) `id`",))
        self.assertEqual(result["status"], "ok")
        value = json.loads(result["stdout"])
        self.assertEqual(value["args"], ["literal $(touch unexpected) `id`"])
        self.assertFalse(value["credential_present"])
        self.assertGreaterEqual(result["elapsed_ms"], 0)
        self.assertEqual(result["measurement"], "fresh_process_wall")

    def test_output_limit_covers_both_pipes_and_stderr_is_not_retained(self):
        result = self.run_script('import sys;sys.stdout.write("x"*200000);sys.stderr.write("y"*200000)', maximum=1024)
        self.assertEqual(result["status"], "output_limit")
        self.assertLessEqual(len(result["stdout"]), 1024)
        self.assertNotIn("stderr", result)

    def test_timeout_and_nonzero_exit_are_failures_not_empty_success(self):
        result = self.run_script('import time;time.sleep(30)', timeout=.15)
        self.assertEqual(result["status"], "timeout")
        self.assertLess(result["elapsed_ms"], 2000)
        result = self.run_script('import sys;sys.exit(7)')
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["returncode"], 7)

    def test_parent_success_still_cleans_its_owned_descendant(self):
        heartbeat = self.root / "heartbeat"
        child = self.root / "owned child.py"
        child.write_text('import time,sys\nwith open(sys.argv[1],"a",buffering=1) as out:\n while True:\n  out.write("x\\n");time.sleep(.01)\n')
        code = ('import subprocess,sys,time\nfrom pathlib import Path\n'
                'p=subprocess.Popen([sys.executable,sys.argv[1],sys.argv[2]],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\n'
                'deadline=time.monotonic()+1\n'
                'while not Path(sys.argv[2]).exists() and time.monotonic()<deadline: time.sleep(.01)\n'
                'print(p.pid)\n')
        result = self.run_script(code, args=(str(child), str(heartbeat)))
        self.assertEqual(result["status"], "ok")
        self.assertTrue(heartbeat.exists())
        before = heartbeat.stat().st_size
        time.sleep(.15)
        self.assertEqual(heartbeat.stat().st_size, before, "owned descendant is still writing after adapter exit")

    def test_invalid_limits_and_missing_executable_refuse(self):
        for timeout in [0, -1, float("inf"), True]:
            with self.assertRaises(ValueError):
                benchmark.owned_command([sys.executable], timeout)
        result = benchmark.owned_command([str(self.root / "missing")], 1)
        self.assertEqual(result["status"], "unavailable")
