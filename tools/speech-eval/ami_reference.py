"""All-speaker AMI meeting references from the NXT manual annotations. Standard library only.

A meeting transcript is scored against every participant's words, not one
headset's speaker (the #118 mistake). For a meeting and a window this builds:

- ``reference``: every agent's words in time order, as one string;
- ``words``: per-word ``{speaker, channel, start_ms, end_ms, text}`` records;
- ``speaker_segments``: per-speaker activity for DER.

Inclusion rule (unchanged from the #118 ``ami_pick.py``): a ``<w>`` element is
used when it is not punctuation (``punc="true"``), has a ``starttime``, and its
midpoint ``(start + end) / 2`` lies in ``[window_start, window_end)``. A missing
``endtime`` counts as the start. Words are ordered by start time, then agent
letter, then file order. Times in the output are milliseconds relative to the
window start (a word straddling the start can be slightly negative).

Speaker activity comes from ``segments/<meeting>.<agent>.segments.xml``
(``transcriber_start``/``transcriber_end``) when that file exists, otherwise
from the union of the agent's timed word intervals. Each speaker's intervals
are unioned, clipped to the window and made window-relative.
"""
import argparse
import json
from pathlib import Path
import sys
import xml.etree.ElementTree as ET

SCHEMA = "ami-meeting-reference-v1"
NITE_ID = "{http://nite.sourceforge.net/}id"


def _ms(seconds):
    return int(round(seconds * 1000))


def _meeting_element(root, meeting):
    tree = ET.parse(Path(root) / "corpusResources" / "meetings.xml").getroot()
    for element in tree.iter("meeting"):
        if element.get("observation") == meeting:
            return element
    raise ValueError(f"meeting {meeting} not in meetings.xml")


def channel_agents(root, meeting):
    """Headset channel number -> NXT agent letter, from corpusResources/meetings.xml."""
    return {int(s.get("channel")): s.get("nxt_agent") for s in _meeting_element(root, meeting).iter("speaker")}


def meeting_duration_s(root, meeting):
    return float(_meeting_element(root, meeting).get("duration"))


def read_words(root, meeting, agent):
    """Timed, non-punctuation words for one agent: [(start_s, end_s, file_index, text)]."""
    path = Path(root) / "words" / f"{meeting}.{agent}.words.xml"
    out = []
    for index, element in enumerate(ET.parse(path).getroot()):
        if element.tag != "w" or element.get("punc") == "true" or not element.get("starttime"):
            continue
        text = (element.text or "").strip()
        if not text:
            continue
        start = float(element.get("starttime"))
        end = float(element.get("endtime") or start)
        out.append((start, max(start, end), index, text))
    return out


def read_segments(root, meeting, agent):
    """Transcriber segments [(start_s, end_s)] for one agent, or None when no file exists."""
    path = Path(root) / "segments" / f"{meeting}.{agent}.segments.xml"
    if not path.exists():
        return None
    spans = []
    for element in ET.parse(path).getroot().iter("segment"):
        start, end = element.get("transcriber_start"), element.get("transcriber_end")
        if start is not None and end is not None:
            spans.append((float(start), float(end)))
    return spans


def union(intervals):
    merged = []
    for start, end in sorted(intervals):
        if end <= start:
            continue
        if merged and start <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    return [tuple(x) for x in merged]


def build_reference(root, meeting, mic_channel, window_s, duration_s=None):
    """Reference document for ``meeting`` cropped to ``window_s = (start, end)`` seconds.

    ``end`` may be None for the full meeting; then ``duration_s`` (the audio
    length) is used, falling back to the meetings.xml duration.
    """
    agents = channel_agents(root, meeting)
    if mic_channel not in agents:
        raise ValueError(f"meeting {meeting} has no headset channel {mic_channel}")
    mic_agent = agents[mic_channel]
    t0, t1 = window_s
    if t1 is None:
        t1 = duration_s if duration_s is not None else meeting_duration_s(root, meeting)
    if not 0 <= t0 < t1:
        raise ValueError("window must satisfy 0 <= start < end")
    speaker = {a: f"{meeting}.{a}" for a in agents.values()}
    channel = {a: "mic" if a == mic_agent else "system" for a in agents.values()}
    rows, sources, segments = [], set(), []
    for agent in sorted(agents.values()):
        words = read_words(root, meeting, agent)
        for start, end, index, text in words:
            if t0 <= (start + end) / 2 < t1:
                rows.append(((start, agent, index), {"speaker": speaker[agent], "channel": channel[agent],
                                                     "start_ms": _ms(start - t0), "end_ms": _ms(end - t0),
                                                     "text": text}))
        spans = read_segments(root, meeting, agent)
        sources.add("segments" if spans is not None else "words")
        if spans is None:
            spans = [(s, e) for s, e, _, _ in words]
        for start, end in union((max(s, t0), min(e, t1)) for s, e in spans):
            segments.append({"speaker": speaker[agent], "channel": channel[agent],
                             "start_ms": _ms(start - t0), "end_ms": _ms(end - t0)})
    rows.sort(key=lambda row: row[0])
    words = [w for _, w in rows]
    segments.sort(key=lambda s: (s["start_ms"], s["speaker"], s["end_ms"]))
    return {
        "schema": SCHEMA,
        "meeting_id": "ami-" + meeting,
        "meetings": [{"meeting": meeting, "offset_ms": 0, "window_s": [t0, t1], "mic_channel": mic_channel,
                      "mic_agent": mic_agent}],
        "duration_ms": _ms(t1 - t0),
        "mic_speakers": [speaker[mic_agent]],
        "system_speakers": sorted(speaker[a] for a in agents.values() if a != mic_agent),
        "speaker_segment_source": "+".join(sorted(sources)),
        "reference": " ".join(w["text"] for w in words),
        "mic_reference": " ".join(w["text"] for w in words if w["channel"] == "mic"),
        "system_reference": " ".join(w["text"] for w in words if w["channel"] == "system"),
        "words": words,
        "speaker_segments": segments,
    }


def concatenate(docs, total_ms):
    """Join window references back to back (as the concatenated audio is), cut at ``total_ms``.

    Each document starts where the previous one's ``duration_ms`` ends. Words are
    kept when their midpoint is before the cut; segments are clipped to it.
    """
    words, segments, meetings, mic, system, sources = [], [], [], [], [], set()
    offset = 0
    for doc in docs:
        if offset >= total_ms:
            break
        for w in doc["words"]:
            start, end = w["start_ms"] + offset, w["end_ms"] + offset
            if (start + end) / 2 < total_ms:
                words.append(dict(w, start_ms=start, end_ms=end))
        for s in doc["speaker_segments"]:
            start, end = s["start_ms"] + offset, min(s["end_ms"] + offset, total_ms)
            if end > start:
                segments.append(dict(s, start_ms=start, end_ms=end))
        for m in doc["meetings"]:
            meetings.append(dict(m, offset_ms=offset + m["offset_ms"],
                                 used_ms=min(doc["duration_ms"], total_ms - offset)))
        mic += [s for s in doc["mic_speakers"] if s not in mic]
        system += [s for s in doc["system_speakers"] if s not in system]
        sources.update(doc["speaker_segment_source"].split("+"))
        offset += doc["duration_ms"]
    if offset < total_ms:
        raise ValueError("documents are shorter than the requested duration")
    return {
        "schema": SCHEMA,
        "meeting_id": "+".join(m["meeting"] for m in meetings),
        "meetings": meetings,
        "duration_ms": total_ms,
        "mic_speakers": mic,
        "system_speakers": system,
        "speaker_segment_source": "+".join(sorted(sources)),
        "reference": " ".join(w["text"] for w in words),
        "mic_reference": " ".join(w["text"] for w in words if w["channel"] == "mic"),
        "system_reference": " ".join(w["text"] for w in words if w["channel"] == "system"),
        "words": words,
        "speaker_segments": segments,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description="Print an all-speaker AMI reference document as JSON.")
    parser.add_argument("--annotations", required=True, help="AMI manual annotations v1.6.2 root")
    parser.add_argument("--meeting", required=True)
    parser.add_argument("--mic-channel", type=int, required=True, help="headset number used as the mic track")
    parser.add_argument("--start", type=float, default=0.0)
    parser.add_argument("--end", type=float, default=None, help="window end in seconds (default: meeting end)")
    args = parser.parse_args(argv)
    doc = build_reference(args.annotations, args.meeting, args.mic_channel, (args.start, args.end))
    json.dump(doc, sys.stdout, ensure_ascii=False, indent=1)
    print()


if __name__ == "__main__":
    main()
