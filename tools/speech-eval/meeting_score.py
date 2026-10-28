"""Meeting WER against every speaker's words, and pyannote-style DER. Standard library only.

Reference documents come from ``ami_reference.py`` / ``ami_replay.py``
(schema ``ami-meeting-reference-v1``). Hypotheses are JSON lists of
``{start_ms, end_ms, text, channel: "mic"|"system", speaker?}`` with times
relative to the window (crop) start.

WER: the combined transcript is every hypothesis segment sorted by
``(start_ms, mic before system, input order)`` and joined; it is scored against
the all-speaker reference with the shared ``speech_eval`` normalization and
alignment. Per-channel WERs (mic hypothesis vs the mic speaker's words, system
hypothesis vs every other speaker's words, both in time order) are diagnostics.

DER: see ``diarization_error_rate``.
"""
import argparse
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from speech_eval import edit_counts, tokenize  # noqa: E402

CHANNELS = ("mic", "system")
DEFAULT_COLLAR_MS = 250
FULL_MEETING_ALIGNMENT_CELLS = 200_000_000


def _int_ms(value, label):
    if type(value) is not int or value < 0:
        raise ValueError(f"{label} must be a nonnegative integer millisecond value")
    return value


def _segments(hypothesis):
    if isinstance(hypothesis, dict) and isinstance(hypothesis.get("segments"), list):
        hypothesis = hypothesis["segments"]
    if not isinstance(hypothesis, list):
        raise ValueError("hypothesis must be a JSON list of segments")
    return hypothesis


def validate_transcript(hypothesis):
    out = []
    for segment in _segments(hypothesis):
        if not isinstance(segment, dict):
            raise ValueError("hypothesis segment must be an object")
        start, end = _int_ms(segment.get("start_ms"), "start_ms"), _int_ms(segment.get("end_ms"), "end_ms")
        if end < start:
            raise ValueError("segment ends before it starts")
        if not isinstance(segment.get("text"), str):
            raise ValueError("segment text is required; missing output is not silence")
        if segment.get("channel") not in CHANNELS:
            raise ValueError('segment channel must be "mic" or "system"')
        if "speaker" in segment and segment["speaker"] is not None and not isinstance(segment["speaker"], str):
            raise ValueError("speaker must be a string")
        out.append(segment)
    return out


def combined_text(hypothesis, channel=None):
    """Hypothesis text in time order; mic before system at equal start, then input order."""
    segments = validate_transcript(hypothesis)
    order = sorted(range(len(segments)),
                   key=lambda i: (segments[i]["start_ms"], CHANNELS.index(segments[i]["channel"]), i))
    return " ".join(segments[i]["text"] for i in order
                    if channel is None or segments[i]["channel"] == channel).strip()


def _wer(reference_text, hypothesis_text, limit):
    result = edit_counts(tokenize(reference_text), tokenize(hypothesis_text), limit=limit)
    return result


def meeting_wer(reference, hypothesis, max_alignment_cells=None):
    """Combined-transcript WER against the all-speaker reference, plus per-channel diagnostics."""
    validate_transcript(hypothesis)
    words = reference["words"]
    ref_text = {None: " ".join(w["text"] for w in words)}
    for channel in CHANNELS:
        ref_text[channel] = " ".join(w["text"] for w in words if w["channel"] == channel)
    return {
        "meeting_id": reference.get("meeting_id"),
        "duration_ms": reference.get("duration_ms"),
        "method": "all-speaker reference; hypothesis segments sorted by start_ms (mic first on ties) and joined",
        "combined": _wer(ref_text[None], combined_text(hypothesis), max_alignment_cells),
        "channels": {channel: _wer(ref_text[channel], combined_text(hypothesis, channel), max_alignment_cells)
                     for channel in CHANNELS},
    }


# ---------------------------------------------------------------- DER

def _union(intervals):
    merged = []
    for start, end in sorted(intervals):
        if end <= start:
            continue
        if merged and start <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    return [tuple(x) for x in merged]


def _subtract(base, holes):
    out = []
    holes = _union(holes)
    for start, end in base:
        cursor = start
        for h0, h1 in holes:
            if h1 <= cursor or h0 >= end:
                continue
            if h0 > cursor:
                out.append((cursor, h0))
            cursor = max(cursor, h1)
        if cursor < end:
            out.append((cursor, end))
    return out


def _by_speaker(segments, uem, label):
    turns = {}
    for segment in segments:
        if not isinstance(segment, dict) or not isinstance(segment.get("speaker"), str) or not segment["speaker"]:
            raise ValueError(f"{label} segment needs a speaker label")
        start, end = _int_ms(segment.get("start_ms"), "start_ms"), _int_ms(segment.get("end_ms"), "end_ms")
        if end < start:
            raise ValueError("segment ends before it starts")
        turns.setdefault(segment["speaker"], []).append((start, end))
    return {speaker: _union(spans) for speaker, spans in turns.items()}


def _clip(turns, uem):
    clipped = {}
    for speaker, spans in turns.items():
        spans = [(max(a, uem[0]), min(b, uem[1])) for a, b in spans if b > uem[0] and a < uem[1]]
        if spans:
            clipped[speaker] = spans
    return clipped


def optimal_mapping(refs, hyps, overlap):
    """One-to-one ref->hyp mapping maximizing total overlap (exact DP over subsets of the smaller side)."""
    swap = len(hyps) < len(refs)
    small, large = (hyps, refs) if swap else (refs, hyps)
    if len(small) > 16:
        raise ValueError("more than 16 speakers on both sides; exact mapping search refused")
    def gain(s, l):
        return overlap.get((l, s) if swap else (s, l), 0)
    best = {0: (0, ())}
    for l in large:
        nxt = dict(best)
        for mask, (value, pairs) in best.items():
            for i, s in enumerate(small):
                if mask & (1 << i):
                    continue
                g = gain(s, l)
                if g <= 0:
                    continue
                key = mask | (1 << i)
                candidate = (value + g, pairs + ((s, l),))
                if key not in nxt or candidate[0] > nxt[key][0]:
                    nxt[key] = candidate
        best = nxt
    value, pairs = max(best.values(), key=lambda x: x[0])
    mapping = {(l if swap else s): (s if swap else l) for s, l in pairs}
    return dict(sorted(mapping.items())), value


def diarization_error_rate(reference, hypothesis, collar_ms=DEFAULT_COLLAR_MS, uem=None):
    """pyannote-style DER on ``[{start_ms, end_ms, speaker}]`` lists.

    - Scored region: ``uem`` (default: the reference extent) minus a forgiveness
      collar of ``collar_ms`` on **each side** of every reference speaker-turn
      boundary (each speaker's turns are unioned first; a turn cut by the
      window edge gets no collar there). This equals
      ``pyannote.metrics`` ``DiarizationErrorRate(collar=2 * collar_ms / 1000)``,
      whose collar is the total width, and NIST md-eval ``-c collar_ms/1000``.
    - Overlapped speech is scored: within each elementary region, with N_ref and
      N_hyp active speakers and N_correct mapped pairs both active,
      missed += max(0, N_ref - N_hyp), false alarm += max(0, N_hyp - N_ref),
      confusion += min(N_ref, N_hyp) - N_correct, total += N_ref (times duration).
    - Speakers are mapped one-to-one to maximize overlap within the scored region
      (the Hungarian optimum, found exactly by DP).
    - DER = (missed + false alarm + confusion) / total.
    """
    ref_all = list(reference)
    if uem is None:
        if not ref_all:
            raise ValueError("an empty reference needs an explicit uem")
        uem = (min(s["start_ms"] for s in ref_all), max(s["end_ms"] for s in ref_all))
    uem = (_int_ms(uem[0], "uem start"), _int_ms(uem[1], "uem end"))
    if uem[1] <= uem[0]:
        raise ValueError("empty scoring window")
    if type(collar_ms) is not int or collar_ms < 0:
        raise ValueError("collar_ms must be a nonnegative integer")
    ref = _by_speaker(ref_all, uem, "reference")
    hyp = _clip(_by_speaker(_segments(hypothesis), uem, "hypothesis"), uem)
    collars = []
    if collar_ms:
        # Collars come from the uncropped turns: a window edge is not a speaker boundary.
        for spans in ref.values():
            for start, end in spans:
                collars += [(start - collar_ms, start + collar_ms), (end - collar_ms, end + collar_ms)]
    ref = _clip(ref, uem)
    scored = _subtract([uem], collars)
    points = {p for span in scored for p in span}
    for turns in (ref, hyp):
        for spans in turns.values():
            points.update(p for span in spans for p in span)
    points = sorted(p for p in points if uem[0] <= p <= uem[1])
    regions = []
    si = 0
    for a, b in zip(points, points[1:]):
        while si < len(scored) and scored[si][1] <= a:
            si += 1
        if si < len(scored) and scored[si][0] <= a and b <= scored[si][1]:
            regions.append((a, b))
    def active(turns):
        cursor = {speaker: 0 for speaker in turns}
        result = []
        for a, b in regions:
            on = set()
            for speaker, spans in turns.items():
                i = cursor[speaker]
                while i < len(spans) and spans[i][1] <= a:
                    i += 1
                cursor[speaker] = i
                if i < len(spans) and spans[i][0] <= a and b <= spans[i][1]:
                    on.add(speaker)
            result.append(on)
        return result
    ref_on, hyp_on = active(ref), active(hyp)
    overlap = {}
    for (a, b), rs, hs in zip(regions, ref_on, hyp_on):
        for r in rs:
            for h in hs:
                overlap[(r, h)] = overlap.get((r, h), 0) + b - a
    mapping, _ = optimal_mapping(sorted(ref), sorted(hyp), overlap)
    missed = false_alarm = confusion = total = 0
    for (a, b), rs, hs in zip(regions, ref_on, hyp_on):
        d = b - a
        correct = sum(1 for r in rs if mapping.get(r) in hs)
        missed += d * max(0, len(rs) - len(hs))
        false_alarm += d * max(0, len(hs) - len(rs))
        confusion += d * (min(len(rs), len(hs)) - correct)
        total += d * len(rs)
    return {
        "der": (missed + false_alarm + confusion) / total if total else None,
        "missed_ms": missed, "false_alarm_ms": false_alarm, "confusion_ms": confusion,
        "total_ms": total, "scored_ms": sum(b - a for a, b in scored),
        "uem_ms": list(uem), "collar_ms": collar_ms,
        "collar_definition": "no scoring within +/- collar_ms of each reference turn boundary "
                             "(pyannote.metrics collar = 2 * collar_ms)",
        "overlap_scored": True,
        "mapping": mapping,
        "reference_speakers": sorted(ref), "hypothesis_speakers": sorted(hyp),
    }


def reference_activity(reference, channel="system", source="segments"):
    """Speaker turns of ``channel`` speakers from a reference document."""
    if source == "segments":
        return [s for s in reference["speaker_segments"] if s["channel"] == channel]
    if source == "words":
        return [{"speaker": w["speaker"], "start_ms": max(0, w["start_ms"]), "end_ms": max(0, w["end_ms"])}
                for w in reference["words"] if w["channel"] == channel]
    raise ValueError("activity source must be segments or words")


def meeting_der(reference, hypothesis, collar_ms=DEFAULT_COLLAR_MS, channel="system", source="segments"):
    """DER of a channel's diarization; hypothesis items whose ``channel`` differs are ignored."""
    hyp = [s for s in _segments(hypothesis)
           if not isinstance(s, dict) or s.get("channel", channel) == channel]
    # Segments without a speaker label (e.g. text-only live segments) are not diarization
    # output; leaving them out scores their speech as missed rather than refusing the file.
    unlabelled = [s for s in hyp if isinstance(s, dict) and s.get("speaker") in (None, "")]
    hyp = [s for s in hyp if not (isinstance(s, dict) and s.get("speaker") in (None, ""))]
    result = diarization_error_rate(reference_activity(reference, channel, source), hyp,
                                    collar_ms=collar_ms, uem=(0, reference["duration_ms"]))
    expected = reference[channel + "_speakers"]
    result.update(meeting_id=reference.get("meeting_id"), channel=channel, reference_activity_source=
                  reference.get("speaker_segment_source") if source == "segments" else "words",
                  reference_speakers_expected=expected, unlabelled_segments_ignored=len(unlabelled))
    result["reference_speakers"] = sorted(result["reference_speakers"])
    return result


def _load(path):
    with open(path, encoding="utf-8") as source:
        return json.load(source)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    wer = sub.add_parser("wer", help="combined all-speaker WER with per-channel diagnostics")
    der = sub.add_parser("der", help="pyannote-style DER of system-track speaker labels")
    for p in (wer, der):
        p.add_argument("--reference", required=True, help="reference JSON from ami_replay.py")
        p.add_argument("--hypothesis", required=True, help="hypothesis JSON list of segments")
    wer.add_argument("--max-alignment-cells", type=int, default=FULL_MEETING_ALIGNMENT_CELLS,
                     help="alignment work limit (ref words x hyp words); default fits full-length meetings")
    der.add_argument("--collar", type=float, default=DEFAULT_COLLAR_MS / 1000,
                     help="forgiveness collar in seconds on each side of reference boundaries (default 0.25)")
    der.add_argument("--channel", choices=CHANNELS, default="system")
    der.add_argument("--activity", choices=("segments", "words"), default="segments",
                     help="reference speaker activity: AMI segments.xml (default) or unioned word times")
    args = parser.parse_args(argv)
    reference, hypothesis = _load(args.reference), _load(args.hypothesis)
    if args.command == "wer":
        result = meeting_wer(reference, hypothesis, args.max_alignment_cells)
    else:
        result = meeting_der(reference, hypothesis, int(round(args.collar * 1000)), args.channel, args.activity)
    json.dump(result, sys.stdout, ensure_ascii=False, indent=1)
    print()


if __name__ == "__main__":
    try:
        main()
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(2)
