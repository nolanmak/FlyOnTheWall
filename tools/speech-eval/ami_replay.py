"""Build two-track AMI meeting replays plus all-speaker references. Standard library only.

For each meeting, one headset is the **mic** track (the local user) and the sum
of the other three headsets is the **system** track (every remote participant,
as a call's system audio carries them). Outputs, under ``--output``:

- ``cropped/``: both tracks cut to the #118 window (default 120-720 s), a
  ``corpus.json`` in speech-eval corpus schema v1, and one reference JSON per
  meeting;
- ``full/``: full-length tracks and references (longer than schema v1's
  30-minute fixture limit, so listed only in the replay manifest);
- ``timing-60min/``: the full-length replays concatenated in meeting order
  (mic with mic, system with system) and cut at exactly 60:00, with the
  matching concatenated reference;
- ``replay-manifest.json``: source URLs and SHA-256s, the mix recipe, clipping
  statistics, output hashes and reference word counts.

Mix recipe: ``system = clip_int16(floor((h_a + h_b + h_c) / 2))`` sample by
sample on the source int16 PCM, where clip saturates to [-32768, 32767]. The
peak of the pre-clip value and the clipped-sample count are recorded. Sources
must already be mono PCM16 16 kHz (AMI headsets are); nothing is resampled and
anything else is refused. The mic track is the source headset copied unchanged.
No audio is written to Git; pick an output directory outside the repository.
"""
import argparse
from array import array
import hashlib
import json
from pathlib import Path
import sys
import wave

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ami_reference  # noqa: E402

RATE = 16_000
CHUNK = RATE * 10
MIRROR = "https://groups.inf.ed.ac.uk/ami/AMICorpusMirror/amicorpus/{m}/audio/{m}.Headset-{h}.wav"
ANNOTATIONS_URL = "https://groups.inf.ed.ac.uk/ami/AMICorpusAnnotations/ami_public_manual_1.6.2.zip"
# Mic headsets match the #118 run's "mic" tracks.
DEFAULT_MEETINGS = (("ES2004c", 2), ("IS1009c", 1), ("TS3003c", 1))
MIX_RECIPE = ("system = clip_int16(floor((sum of the three non-mic headsets) / 2)); "
              "mic = mic headset unchanged; mono PCM16 16 kHz in and out, no resampling")


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def open_source(path):
    reader = wave.open(str(path), "rb")
    if (reader.getnchannels(), reader.getsampwidth(), reader.getframerate(), reader.getcomptype()) != (1, 2, RATE, "NONE"):
        reader.close()
        raise ValueError(f"{path}: expected mono PCM16 16 kHz (resampling is not supported)")
    return reader


def writer(path):
    out = wave.open(str(path), "wb")
    out.setnchannels(1)
    out.setsampwidth(2)
    out.setframerate(RATE)
    return out


def samples(data):
    values = array("h")
    values.frombytes(data)
    if sys.byteorder != "little":
        values.byteswap()
    return values


def to_bytes(values):
    if sys.byteorder != "little":
        values = array("h", values)
        values.byteswap()
    return values.tobytes()


def mix(chunks):
    """floor(sum / 2), saturated to int16. Returns (array, peak_abs_preclip, clipped_count)."""
    summed = [sum(v) >> 1 for v in zip(*chunks)]
    lo, hi = min(summed, default=0), max(summed, default=0)
    clipped = 0
    if lo < -32768 or hi > 32767:
        clipped = sum(1 for v in summed if v < -32768 or v > 32767)
        summed = [-32768 if v < -32768 else 32767 if v > 32767 else v for v in summed]
    return array("h", summed), max(-lo, hi), clipped


def build_full(audio_dir, out_dir, meeting, mic_channel, channels):
    """Write <meeting>-mic.wav and <meeting>-system.wav at full length."""
    paths = {h: Path(audio_dir) / f"{meeting}.Headset-{h}.wav" for h in channels}
    readers = {h: open_source(p) for h, p in paths.items()}
    try:
        frames = {h: r.getnframes() for h, r in readers.items()}
        length = min(frames.values())
        others = [h for h in sorted(channels) if h != mic_channel]
        mic_path, system_path = out_dir / f"{meeting}-mic.wav", out_dir / f"{meeting}-system.wav"
        peak = clipped = 0
        with writer(mic_path) as mic_out, writer(system_path) as system_out:
            done = 0
            while done < length:
                n = min(CHUNK, length - done)
                data = {h: readers[h].readframes(n) for h in channels}
                if any(len(d) != 2 * n for d in data.values()):
                    raise ValueError(f"{meeting}: truncated source PCM")
                mic_out.writeframes(data[mic_channel])
                mixed, p, c = mix([samples(data[h]) for h in others])
                system_out.writeframes(to_bytes(mixed))
                peak, clipped = max(peak, p), clipped + c
                done += n
    finally:
        for r in readers.values():
            r.close()
    sources = [{"meeting": meeting, "headset": h, "track": "mic" if h == mic_channel else "system",
                "url": MIRROR.format(m=meeting, h=h), "sha256": sha256(paths[h]), "frames": frames[h],
                "sample_rate": RATE} for h in sorted(channels)]
    stats = {"frames": length, "system_peak_abs_preclip": peak, "system_clipped_samples": clipped,
             "system_headsets": others, "mic_headset": mic_channel,
             "frames_trimmed_to_shortest_headset": {str(h): frames[h] - length for h in sorted(channels)}}
    return mic_path, system_path, sources, stats


def copy_frames(sources, destination):
    """Concatenate frame ranges [(path, start, count), ...] into destination."""
    with writer(destination) as out:
        for path, first, n in sources:
            with wave.open(str(path), "rb") as reader:
                reader.setpos(first)
                left = n
                while left:
                    data = reader.readframes(min(CHUNK, left))
                    if not data:
                        raise ValueError(f"{path}: shorter than requested")
                    out.writeframes(data)
                    left -= len(data) // 2


def write_json(path, doc):
    path.write_text(json.dumps(doc, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")


def word_counts(doc):
    from speech_eval import tokenize
    return {"all_speakers": len(tokenize(doc["reference"])), "mic_speaker": len(tokenize(doc["mic_reference"])),
            "system_speakers": len(tokenize(doc["system_reference"])), "annotation_words": len(doc["words"])}


def output_entry(root, mic, system, reference, doc):
    return {"meeting_id": doc["meeting_id"], "duration_ms": doc["duration_ms"],
            "mic": {"audio": str(mic.relative_to(root)), "sha256": sha256(mic)},
            "system": {"audio": str(system.relative_to(root)), "sha256": sha256(system)},
            "reference": str(reference.relative_to(root)), "reference_sha256": sha256(reference),
            "reference_words": word_counts(doc), "mic_speakers": doc["mic_speakers"],
            "system_speakers": doc["system_speakers"], "speaker_segment_source": doc["speaker_segment_source"]}


def build(audio_dir, annotations, output, meetings=DEFAULT_MEETINGS, window=(120, 720), timing_s=3600):
    output = Path(output)
    full_dir, crop_dir, timing_dir = output / "full", output / "cropped", output / "timing-60min"
    for d in (full_dir, crop_dir, timing_dir):
        d.mkdir(parents=True, exist_ok=True)
    zip_path = Path(annotations) / "ami_public_manual_1.6.2.zip"
    manifest = {
        "schema": "ami-meeting-replay-v1",
        "license": "CC-BY-4.0 (AMI Meeting Corpus, audio and manual annotations v1.6.2)",
        "annotations": {"url": ANNOTATIONS_URL, "sha256": sha256(zip_path) if zip_path.exists() else None},
        "mix_recipe": MIX_RECIPE,
        "reference_method": "every agent's timed non-punctuation <w> with midpoint in [start, end), ordered by "
                            "start time, agent, file order (tools/speech-eval/ami_reference.py)",
        "window_s": list(window), "sources": [], "full": [], "cropped": [], "timing": None,
    }
    fixtures, full_docs, full_audio = [], [], []
    for meeting, mic_channel in meetings:
        channels = sorted(ami_reference.channel_agents(annotations, meeting))
        mic, system, sources, stats = build_full(audio_dir, full_dir, meeting, mic_channel, channels)
        manifest["sources"] += sources
        frames = stats["frames"]
        full_doc = ami_reference.build_reference(annotations, meeting, mic_channel, (0.0, None), duration_s=frames / RATE)
        full_ref = full_dir / f"{meeting}.reference.json"
        write_json(full_ref, full_doc)
        manifest["full"].append(dict(output_entry(output, mic, system, full_ref, full_doc), mix=stats))
        full_docs.append(full_doc)
        full_audio.append((mic, system, frames))

        t0, t1 = window
        first, count = int(t0 * RATE), int((t1 - t0) * RATE)
        if first + count > frames:
            raise ValueError(f"{meeting}: crop window past end of audio")
        crop_mic, crop_system = crop_dir / f"{meeting}-mic.wav", crop_dir / f"{meeting}-system.wav"
        copy_frames([(mic, first, count)], crop_mic)
        copy_frames([(system, first, count)], crop_system)
        crop_doc = ami_reference.build_reference(annotations, meeting, mic_channel, (float(t0), float(t1)))
        crop_ref = crop_dir / f"{meeting}.reference.json"
        write_json(crop_ref, crop_doc)
        entry = output_entry(output, crop_mic, crop_system, crop_ref, crop_doc)
        manifest["cropped"].append(entry)
        for track, path in (("mic", crop_mic), ("system", crop_system)):
            others = [s["headset"] for s in sources if s["track"] == "system"]
            heads = str(mic_channel) if track == "mic" else "+".join(map(str, others))
            fixtures.append({
                "id": f"ami2-{meeting}-{track}", "audio": path.name, "sha256": entry[track]["sha256"],
                "reference": crop_doc["reference"], "terms": [], "kind": "human", "track": track,
                "meeting_id": crop_doc["meeting_id"],
                "tags": ["ami", "meeting", "all-speaker-reference", f"crop-{t0}-{t1}s"],
                "license": "CC-BY-4.0",
                "source_url": MIRROR.format(m=meeting, h=mic_channel) if track == "mic"
                else f"https://groups.inf.ed.ac.uk/ami/AMICorpusMirror/amicorpus/{meeting}/audio/",
                "source_revision": f"headset {heads} ({'copy' if track == 'mic' else 'sum/2 clip'}) crop {t0}-{t1}s; "
                                   "source sha256s in ../replay-manifest.json"})
    write_json(crop_dir / "corpus.json", {"schema_version": 1, "fixtures": fixtures})

    total = timing_s * RATE
    for track, index in (("mic", 0), ("system", 1)):
        parts, left = [], total
        for audio in full_audio:
            if left <= 0:
                break
            n = min(audio[2], left)
            parts.append((audio[index], 0, n))
            left -= n
        if left:
            raise ValueError("meetings are shorter than the timing fixture")
        copy_frames(parts, timing_dir / f"{track}.wav")
    timing_doc = ami_reference.concatenate(full_docs, timing_s * 1000)
    timing_ref = timing_dir / "reference.json"
    write_json(timing_ref, timing_doc)
    manifest["timing"] = output_entry(output, timing_dir / "mic.wav", timing_dir / "system.wav", timing_ref, timing_doc)
    manifest["timing"]["parts"] = timing_doc["meetings"]
    write_json(output / "replay-manifest.json", manifest)
    return manifest


def main(argv=None):
    parser = argparse.ArgumentParser(description="Build two-track AMI meeting replays and all-speaker references.")
    parser.add_argument("--audio-dir", required=True, help="directory with <meeting>.Headset-<n>.wav for all headsets")
    parser.add_argument("--annotations", required=True, help="AMI manual annotations v1.6.2 root")
    parser.add_argument("--output", required=True, help="output directory (outside Git)")
    parser.add_argument("--meeting", action="append", metavar="MEETING:MIC_HEADSET",
                        help="default: ES2004c:2 IS1009c:1 TS3003c:1 (the #118 mic headsets)")
    parser.add_argument("--start", type=float, default=120.0)
    parser.add_argument("--end", type=float, default=720.0)
    parser.add_argument("--timing-seconds", type=int, default=3600)
    args = parser.parse_args(argv)
    meetings = DEFAULT_MEETINGS if not args.meeting else tuple(
        (m.split(":")[0], int(m.split(":")[1])) for m in args.meeting)
    window = tuple(int(x) if float(x).is_integer() else x for x in (args.start, args.end))
    manifest = build(args.audio_dir, args.annotations, args.output, meetings, window, args.timing_seconds)
    summary = {kind: [{"meeting_id": e["meeting_id"], "duration_ms": e["duration_ms"],
                       "reference_words": e["reference_words"]} for e in manifest[kind]] for kind in ("cropped", "full")}
    summary["timing"] = {"duration_ms": manifest["timing"]["duration_ms"],
                         "reference_words": manifest["timing"]["reference_words"]}
    summary["system_clipping"] = {e["meeting_id"]: {"peak": e["mix"]["system_peak_abs_preclip"],
                                                    "clipped": e["mix"]["system_clipped_samples"]} for e in manifest["full"]}
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
