"""All-speaker AMI reference construction on synthetic NXT XML (no AMI data in Git)."""
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
import ami_reference

NITE = 'xmlns:nite="http://nite.sourceforge.net/"'


def words_xml(meeting, agent, items):
    body = []
    for n, item in enumerate(items):
        kind, start, end, text = item
        ident = f'nite:id="{meeting}.{agent}.words{n}"'
        times = "" if start is None else f' starttime="{start}" endtime="{end}"'
        if kind == "w":
            body.append(f"<w {ident}{times}>{text}</w>")
        elif kind == "punc":
            body.append(f'<w {ident}{times} punc="true">{text}</w>')
        else:
            body.append(f'<{kind} {ident}{times}/>')
    return (f'<?xml version="1.0" encoding="ISO-8859-1"?>\n<nite:root nite:id="{meeting}.{agent}.words" {NITE}>'
            + "".join(body) + "</nite:root>")


def segments_xml(meeting, agent, spans):
    body = "".join(f'<segment nite:id="{meeting}.sync.{n}" transcriber_start="{s}" transcriber_end="{e}"/>'
                   for n, (s, e) in enumerate(spans))
    return f'<?xml version="1.0"?>\n<nite:root nite:id="{meeting}.{agent}.segs" {NITE}>{body}</nite:root>'


MEETINGS_XML = f"""<?xml version="1.0"?>
<nite:root {NITE}>
 <meeting nite:id="m1" observation="XX0001a" duration="30">
  <speaker channel="1" nxt_agent="B" global_name="P2"/>
  <speaker channel="0" nxt_agent="A" global_name="P1"/>
  <speaker channel="3" nxt_agent="D" global_name="P4"/>
  <speaker channel="2" nxt_agent="C" global_name="P3"/>
 </meeting>
</nite:root>"""


class AmiReferenceTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        (self.root / "words").mkdir()
        (self.root / "segments").mkdir()
        (self.root / "corpusResources").mkdir()
        (self.root / "corpusResources" / "meetings.xml").write_text(MEETINGS_XML)
        m = "XX0001a"
        files = {
            # The mic speaker (headset 1 = agent B).
            "B": [("w", 10.0, 10.4, "hello"), ("punc", 10.4, 10.4, ","),
                  ("w", 12.0, 12.2, "same"), ("w", 12.0, 12.1, "start"),
                  ("w", None, None, "untimed"), ("w", 19.7, 20.1, "late")],
            "A": [("w", 9.8, 10.1, "edge"),          # midpoint 9.95: before the window
                  ("w", 9.9, 10.1, "boundary"),      # midpoint 10.0: inside (start inclusive)
                  ("vocalsound", 11.0, 11.5, None),
                  ("w", 12.0, 12.3, "tie"),          # same start as B's "same": agent A first
                  ("w", 19.8, 20.2, "outside")],     # midpoint 20.0: excluded (end exclusive)
            "C": [("w", 11.0, 11.2, "middle"), ("punc", 11.2, 11.2, ".")],
            "D": [("w", 15.0, 15.5, "café"), ("disfmarker", 15.5, 15.5, None)],
        }
        for agent, items in files.items():
            (self.root / "words" / f"{m}.{agent}.words.xml").write_text(words_xml(m, agent, items), encoding="iso-8859-1")
        (self.root / "segments" / f"{m}.A.segments.xml").write_text(segments_xml(m, "A", [(5.0, 10.5), (12.0, 13.0), (19.0, 25.0)]))
        (self.root / "segments" / f"{m}.C.segments.xml").write_text(segments_xml(m, "C", [(11.0, 11.4), (11.2, 12.0)]))
        (self.root / "segments" / f"{m}.D.segments.xml").write_text(segments_xml(m, "D", [(1.0, 2.0)]))
        self.meeting = m

    def test_channel_to_agent_map_comes_from_meetings_xml(self):
        self.assertEqual(ami_reference.channel_agents(self.root, self.meeting), {0: "A", 1: "B", 2: "C", 3: "D"})

    def test_reference_merges_every_speaker_in_time_order_with_crop_rule(self):
        doc = ami_reference.build_reference(self.root, self.meeting, mic_channel=1, window_s=(10.0, 20.0))
        self.assertEqual(doc["reference"], "boundary hello middle tie same start café late")
        self.assertEqual(doc["mic_speakers"], ["XX0001a.B"])
        self.assertEqual(doc["system_speakers"], ["XX0001a.A", "XX0001a.C", "XX0001a.D"])
        self.assertEqual(doc["duration_ms"], 10_000)
        first = doc["words"][0]
        self.assertEqual(first, {"speaker": "XX0001a.A", "channel": "system", "start_ms": -100, "end_ms": 100,
                                 "text": "boundary"})
        self.assertEqual([w["channel"] for w in doc["words"]],
                         ["system", "mic", "system", "system", "mic", "mic", "system", "mic"])
        self.assertEqual(doc["mic_reference"], "hello same start late")
        self.assertEqual(doc["system_reference"], "boundary middle tie café")

    def test_full_length_window_defaults_to_meeting_duration(self):
        doc = ami_reference.build_reference(self.root, self.meeting, mic_channel=1, window_s=(0.0, None),
                                            duration_s=30.0)
        self.assertEqual(doc["duration_ms"], 30_000)
        self.assertEqual(doc["reference"].split(), ["edge", "boundary", "hello", "middle", "tie", "same", "start",
                                                    "café", "late", "outside"])

    def test_speaker_segments_are_merged_clipped_and_window_relative(self):
        doc = ami_reference.build_reference(self.root, self.meeting, mic_channel=1, window_s=(10.0, 20.0))
        # A, C and D have segments files; B falls back to its timed word intervals.
        self.assertEqual(doc["speaker_segment_source"], "segments+words")
        system = [(s["speaker"], s["start_ms"], s["end_ms"]) for s in doc["speaker_segments"] if s["channel"] == "system"]
        self.assertEqual(system, [("XX0001a.A", 0, 500), ("XX0001a.C", 1000, 2000), ("XX0001a.A", 2000, 3000),
                                  ("XX0001a.A", 9000, 10000)])
        mic = [(s["start_ms"], s["end_ms"]) for s in doc["speaker_segments"] if s["channel"] == "mic"]
        self.assertEqual(mic, [(0, 400), (2000, 2200), (9700, 10000)])

    def test_concatenation_offsets_meetings_and_keeps_speakers_distinct(self):
        one = ami_reference.build_reference(self.root, self.meeting, mic_channel=1, window_s=(10.0, 20.0))
        joined = ami_reference.concatenate([one, one], total_ms=15_000)
        self.assertEqual(joined["duration_ms"], 15_000)
        starts = [w["start_ms"] for w in joined["words"]]
        self.assertEqual(starts, sorted(starts))
        # Second copy is offset by 10 s and cut at 15 s (word midpoint rule).
        self.assertEqual(joined["reference"], "boundary hello middle tie same start café late boundary hello middle tie same start")
        self.assertTrue(all(s["end_ms"] <= 15_000 for s in joined["speaker_segments"]))


if __name__ == "__main__":
    unittest.main()
