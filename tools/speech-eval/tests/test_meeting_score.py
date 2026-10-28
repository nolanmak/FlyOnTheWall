"""Meeting WER against every speaker's words, and DER, on hand-computed cases."""
import itertools
import json
from pathlib import Path
import random
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).parents[1]
sys.path.insert(0, str(HERE))
import meeting_score


def word(speaker, channel, start, end, text):
    return {"speaker": speaker, "channel": channel, "start_ms": start, "end_ms": end, "text": text}


REFERENCE = {
    "schema": "ami-meeting-reference-v1",
    "meeting_id": "ami-XX0001a",
    "duration_ms": 10_000,
    "mic_speakers": ["M"],
    "system_speakers": ["S1", "S2"],
    "words": [word("M", "mic", 0, 300, "hello"),
              word("S1", "system", 500, 600, "how"), word("S2", "system", 700, 800, "are"),
              word("S1", "system", 900, 1000, "you"),
              word("M", "mic", 2000, 2200, "fine"), word("M", "mic", 2300, 2500, "thanks")],
    "speaker_segments": [{"speaker": "S1", "channel": "system", "start_ms": 0, "end_ms": 6000},
                         {"speaker": "S2", "channel": "system", "start_ms": 6000, "end_ms": 10_000},
                         {"speaker": "M", "channel": "mic", "start_ms": 0, "end_ms": 10_000}],
}


def seg(start, end, speaker):
    return {"start_ms": start, "end_ms": end, "speaker": speaker}


class MeetingWerTests(unittest.TestCase):
    def test_combined_transcript_is_time_ordered_across_channels(self):
        hypothesis = [{"start_ms": 400, "end_ms": 1000, "text": "how are you", "channel": "system", "speaker": "x"},
                      {"start_ms": 0, "end_ms": 300, "text": "Hello", "channel": "mic"},
                      {"start_ms": 1900, "end_ms": 2500, "text": "fine tanks", "channel": "mic"}]
        result = meeting_score.meeting_wer(REFERENCE, hypothesis)
        combined = result["combined"]
        self.assertEqual((combined["reference_words"], combined["substitutions"], combined["deletions"],
                          combined["insertions"]), (6, 1, 0, 0))
        self.assertAlmostEqual(combined["wer"], 1 / 6)
        self.assertAlmostEqual(result["channels"]["mic"]["wer"], 1 / 3)
        self.assertEqual(result["channels"]["system"]["errors"], 0)
        self.assertEqual(result["channels"]["system"]["reference_words"], 3)

    def test_missing_remote_speech_counts_as_deletions(self):
        # The #118 per-headset method would have scored this mic-only output as perfect.
        hypothesis = [{"start_ms": 0, "end_ms": 2500, "text": "hello fine thanks", "channel": "mic"}]
        result = meeting_score.meeting_wer(REFERENCE, hypothesis)
        self.assertEqual(result["combined"]["deletions"], 3)
        self.assertAlmostEqual(result["combined"]["wer"], 0.5)
        self.assertEqual(result["channels"]["mic"]["errors"], 0)
        self.assertEqual(result["channels"]["system"]["deletions"], 3)

    def test_equal_start_orders_mic_before_system_then_input_order(self):
        hypothesis = [{"start_ms": 0, "end_ms": 100, "text": "b", "channel": "system"},
                      {"start_ms": 0, "end_ms": 100, "text": "a", "channel": "mic"},
                      {"start_ms": 0, "end_ms": 100, "text": "c", "channel": "system"}]
        self.assertEqual(meeting_score.combined_text(hypothesis), "a b c")

    def test_invalid_hypothesis_is_rejected_not_scored_as_silence(self):
        for bad in [[{"start_ms": 0, "end_ms": 1, "text": "x", "channel": "both"}],
                    [{"start_ms": 5, "end_ms": 1, "text": "x", "channel": "mic"}],
                    [{"start_ms": 0, "end_ms": 1, "channel": "mic"}],
                    [{"start_ms": 0.5, "end_ms": 1, "text": "x", "channel": "mic"}],
                    {"text": "x"}]:
            with self.assertRaises(ValueError):
                meeting_score.meeting_wer(REFERENCE, bad)


class DerTests(unittest.TestCase):
    def der(self, ref, hyp, collar_ms=250, uem=(0, 10_000)):
        return meeting_score.diarization_error_rate(ref, hyp, collar_ms=collar_ms, uem=uem)

    def test_relabelled_perfect_hypothesis_scores_zero(self):
        result = self.der([seg(0, 6000, "A"), seg(6000, 10_000, "B")],
                          [seg(0, 6000, "spk1"), seg(6000, 10_000, "spk0")], collar_ms=0)
        self.assertEqual(result["der"], 0)
        self.assertEqual(result["mapping"], {"A": "spk1", "B": "spk0"})

    def test_collar_forgives_each_side_of_every_reference_boundary(self):
        hidden = self.der([seg(0, 10_000, "A")], [seg(200, 10_000, "X")])
        self.assertEqual((hidden["missed_ms"], hidden["total_ms"], hidden["der"]), (0, 9500, 0))
        self.assertEqual(hidden["scored_ms"], 9500)
        visible = self.der([seg(0, 10_000, "A")], [seg(500, 10_000, "X")])
        self.assertEqual(visible["missed_ms"], 250)
        self.assertAlmostEqual(visible["der"], 250 / 9500)

    def test_overlapped_speech_is_scored(self):
        result = self.der([seg(0, 10_000, "A"), seg(5000, 10_000, "B")], [seg(0, 10_000, "X")])
        # Collars remove [0,250], [4750,5250], [9750,10000]; B is missed for 4.5 s.
        self.assertEqual((result["missed_ms"], result["false_alarm_ms"], result["confusion_ms"], result["total_ms"]),
                         (4500, 0, 0, 13_500))
        self.assertAlmostEqual(result["der"], 1 / 3)
        self.assertTrue(result["overlap_scored"])

    def test_confusion_uses_optimal_one_to_one_mapping(self):
        result = self.der([seg(0, 6000, "A"), seg(6000, 10_000, "B")],
                          [seg(0, 4000, "X"), seg(4000, 10_000, "Y")], collar_ms=0)
        self.assertEqual(result["mapping"], {"A": "X", "B": "Y"})
        self.assertEqual((result["confusion_ms"], result["missed_ms"], result["false_alarm_ms"]), (2000, 0, 0))
        self.assertAlmostEqual(result["der"], 0.2)

    def test_extra_hypothesis_speaker_is_false_alarm_inside_and_outside_speech(self):
        result = self.der([seg(0, 4000, "A")], [seg(0, 4000, "X"), seg(2000, 6000, "Y")], collar_ms=0, uem=(0, 6000))
        self.assertEqual((result["false_alarm_ms"], result["confusion_ms"], result["total_ms"]), (4000, 0, 4000))
        self.assertEqual(result["der"], 1.0)

    def test_scoring_is_limited_to_the_window_and_same_speaker_overlap_is_merged(self):
        result = self.der([seg(0, 1000, "A")], [seg(0, 600, "X"), seg(500, 1000, "X"), seg(5000, 9000, "Z")],
                          collar_ms=0, uem=(0, 2000))
        self.assertEqual(result["der"], 0)

    def test_mapping_search_matches_brute_force(self):
        rng = random.Random(246)
        for _ in range(60):
            refs = [f"r{i}" for i in range(rng.randint(1, 4))]
            hyps = [f"h{i}" for i in range(rng.randint(1, 5))]
            overlap = {(r, h): rng.randint(0, 20) for r in refs for h in hyps}
            _, best = meeting_score.optimal_mapping(refs, hyps, overlap)
            brute = 0
            k = min(len(refs), len(hyps))
            for rs in itertools.permutations(refs, k):
                for hs in itertools.permutations(hyps, k):
                    brute = max(brute, sum(overlap[p] for p in zip(rs, hs)))
            self.assertEqual(best, brute)


class CliTests(unittest.TestCase):
    def test_wer_and_der_commands_emit_json(self):
        with tempfile.TemporaryDirectory() as tmp:
            ref = Path(tmp) / "ref.json"
            hyp = Path(tmp) / "hyp.json"
            ref.write_text(json.dumps(REFERENCE))
            hyp.write_text(json.dumps([
                {"start_ms": 0, "end_ms": 2500, "text": "hello fine thanks", "channel": "mic"},
                {"start_ms": 0, "end_ms": 6000, "text": "how are you", "channel": "system", "speaker": "a"},
                {"start_ms": 6000, "end_ms": 10_000, "text": "", "channel": "system", "speaker": "b"}]))
            script = str(HERE / "meeting_score.py")
            wer = json.loads(subprocess.run([sys.executable, "-I", script, "wer", "--reference", str(ref),
                                             "--hypothesis", str(hyp)], check=True, capture_output=True).stdout)
            self.assertEqual(wer["combined"]["errors"], 0)
            der = json.loads(subprocess.run([sys.executable, "-I", script, "der", "--reference", str(ref),
                                             "--hypothesis", str(hyp), "--collar", "0"],
                                            check=True, capture_output=True).stdout)
            self.assertEqual(der["der"], 0)
            self.assertEqual(der["reference_speakers"], ["S1", "S2"])


if __name__ == "__main__":
    unittest.main()
