"""Two-track replay builder on synthetic headsets and annotations (no AMI data in Git)."""
from array import array
import json
from pathlib import Path
import sys
import tempfile
import unittest
import wave

sys.path.insert(0, str(Path(__file__).parents[1]))
sys.path.insert(0, str(Path(__file__).parent))
import ami_replay
import benchmark
from test_ami_reference import MEETINGS_XML, words_xml

RATE = 16_000


def write_wav(path, values):
    with wave.open(str(path), "wb") as out:
        out.setnchannels(1)
        out.setsampwidth(2)
        out.setframerate(RATE)
        out.writeframes(array("h", values).tobytes())


def read_wav(path):
    with wave.open(str(path), "rb") as source:
        values = array("h")
        values.frombytes(source.readframes(source.getnframes()))
        return list(values)


class ReplayTests(unittest.TestCase):
    def test_mix_floors_half_the_sum_and_counts_clipping(self):
        mixed, peak, clipped = ami_replay.mix([array("h", [3, 30000, -30000, -3]),
                                               array("h", [0, 30000, -30000, 0]),
                                               array("h", [0, 20000, -20000, 0])])
        self.assertEqual(list(mixed), [1, 32767, -32768, -2])
        self.assertEqual((peak, clipped), (40000, 2))

    def test_build_writes_tracks_references_and_schema_v1_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            annot, audio, out = root / "annot", root / "audio", root / "out"
            for d in (annot / "words", annot / "segments", annot / "corpusResources", audio):
                d.mkdir(parents=True)
            (annot / "corpusResources" / "meetings.xml").write_text(MEETINGS_XML)
            m = "XX0001a"
            for agent, text, start in (("A", "alpha", 1.2), ("B", "bravo", 1.4), ("C", "charlie", 1.6), ("D", "delta", 2.5)):
                (annot / "words" / f"{m}.{agent}.words.xml").write_text(words_xml(m, agent, [("w", start, start + 0.2, text)]))
            seconds = 4
            for headset in range(4):
                write_wav(audio / f"{m}.Headset-{headset}.wav",
                          [(headset + 1) * 1000 + (i % 7) for i in range(seconds * RATE)])
            manifest = ami_replay.build(audio, annot, out, meetings=((m, 1),), window=(1, 2), timing_s=3)

            mic, system = read_wav(out / "full" / f"{m}-mic.wav"), read_wav(out / "full" / f"{m}-system.wav")
            self.assertEqual(len(mic), seconds * RATE)
            self.assertEqual(mic[:3], [2000, 2001, 2002])  # headset 1 unchanged
            self.assertEqual(system[:3], [(1000 + 3000 + 4000) // 2, (1001 + 3001 + 4001) // 2, (1002 + 3002 + 4002) // 2])
            self.assertEqual(read_wav(out / "cropped" / f"{m}-system.wav"), system[RATE:2 * RATE])
            self.assertEqual(read_wav(out / "timing-60min" / "mic.wav"), mic[:3 * RATE])

            crop_ref = json.loads((out / "cropped" / f"{m}.reference.json").read_text())
            self.assertEqual(crop_ref["reference"], "alpha bravo charlie")
            self.assertEqual(crop_ref["mic_reference"], "bravo")
            self.assertEqual(manifest["cropped"][0]["reference_words"]["all_speakers"], 3)
            self.assertEqual(manifest["full"][0]["reference_words"]["all_speakers"], 4)
            self.assertEqual(manifest["full"][0]["mix"]["system_clipped_samples"], 0)
            self.assertEqual([s["headset"] for s in manifest["sources"]], [0, 1, 2, 3])
            self.assertTrue(all(s["url"].startswith("https://") and len(s["sha256"]) == 64 for s in manifest["sources"]))

            corpus = benchmark.load_corpus(out / "cropped" / "corpus.json")
            self.assertEqual([(f["track"], f["meeting_id"], f["duration_ms"]) for f in corpus["fixtures"]],
                             [("mic", "ami-XX0001a", 1000), ("system", "ami-XX0001a", 1000)])
            self.assertTrue(all(f["reference"] == "alpha bravo charlie" for f in corpus["fixtures"]))


if __name__ == "__main__":
    unittest.main()
