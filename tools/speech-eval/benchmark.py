"""Validated benchmark corpus and adapter protocol. Never downloads assets."""
import hashlib
import json
import math
from pathlib import Path
import re
from urllib.parse import urlsplit
import wave

from speech_eval import score, tokenize

MAX_AUDIO_BYTES = 60_000_000
MAX_RESPONSE_BYTES = 1_048_576
ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,79}\Z")


def strict_json(raw):
    def invalid_constant(_value):
        raise ValueError("nonfinite JSON number")
    def floating(value):
        result = float(value)
        if not math.isfinite(result):
            raise ValueError("nonfinite JSON number")
        return result
    def integer(value):
        if len(value.lstrip("-")) > 20:
            raise ValueError("JSON integer exceeds measurement range")
        return int(value)
    def object_pairs(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate JSON field")
            result[key] = value
        return result
    return json.loads(raw, parse_constant=invalid_constant, parse_float=floating,
                      parse_int=integer, object_pairs_hook=object_pairs)


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1_048_576), b""):
            digest.update(chunk)
    return digest.hexdigest()


def nonempty(value, label, maximum=500):
    if not isinstance(value, str) or not value.strip() or len(value) > maximum:
        raise ValueError("missing or invalid " + label)
    return value


def finite(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
        raise ValueError("expected finite nonnegative measurement")
    return value


def wav_duration(path):
    try:
        with wave.open(str(path), "rb") as audio:
            if (audio.getnchannels(), audio.getsampwidth(), audio.getframerate(), audio.getcomptype()) != (1, 2, 16000, "NONE"):
                raise ValueError("expected mono PCM16 16kHz WAV")
            frames = audio.getnframes()
            if not 0 < frames <= 16000 * 1800:
                raise ValueError("audio duration outside 0–30 minutes")
            actual = 0
            while True:
                data = audio.readframes(16000)
                if not data:
                    break
                actual += len(data)
            if actual != frames * 2:
                raise ValueError("truncated PCM payload")
            return frames * 1000 / 16000
    except (wave.Error, EOFError, OSError) as error:
        raise ValueError("invalid or unreadable WAV") from error


def load_corpus(path):
    path = Path(path)
    if path.stat().st_size > 4_194_304:
        raise ValueError("corpus manifest too large")
    raw = path.read_bytes()
    doc = strict_json(raw)
    if not isinstance(doc, dict) or set(doc) != {"schema_version", "fixtures"} or type(doc["schema_version"]) is not int or doc["schema_version"] != 1:
        raise ValueError("unsupported corpus schema")
    if not isinstance(doc["fixtures"], list) or not 0 < len(doc["fixtures"]) <= 1000:
        raise ValueError("expected 1–1000 explicitly licensed fixtures")
    root = path.resolve().parent
    fixtures, seen = [], set()
    allowed = {"id", "audio", "sha256", "reference", "terms", "kind", "track", "tags", "license", "source_url", "source_revision", "meeting_id"}
    for item in doc["fixtures"]:
        if not isinstance(item, dict) or set(item) - allowed:
            raise ValueError("unknown fixture field")
        identity = nonempty(item.get("id"), "fixture id", 80)
        if not ID.fullmatch(identity) or identity in seen:
            raise ValueError("duplicate or invalid fixture id")
        seen.add(identity)
        relative = Path(nonempty(item.get("audio"), "audio path"))
        try:
            audio = (root / relative).resolve(strict=True)
        except OSError as error:
            raise ValueError("missing fixture audio") from error
        if relative.is_absolute() or root not in audio.parents or not audio.is_file() or audio.stat().st_size > MAX_AUDIO_BYTES:
            raise ValueError("audio must stay inside the corpus directory and size limit")
        expected = item.get("sha256")
        if not isinstance(expected, str) or not re.fullmatch(r"[0-9a-f]{64}", expected) or sha256_file(audio) != expected:
            raise ValueError("fixture hash mismatch")
        reference = item.get("reference")
        if not isinstance(reference, str) or len(reference) > 200_000 or len(tokenize(reference)) > 12_000:
            raise ValueError("invalid reference text")
        terms = item.get("terms", [])
        score(reference, "", terms)  # validates annotations before any model runs
        if item.get("kind") not in {"human", "synthetic", "silence"} or item.get("track") not in {"utterance", "mic", "system"}:
            raise ValueError("fixture speech kind and source track are required")
        if item["kind"] == "silence" and tokenize(reference):
            raise ValueError("silence fixture cannot contain reference speech")
        tags = item.get("tags")
        if not isinstance(tags, list) or not tags or len(tags) > 16 or any(not isinstance(tag, str) or not ID.fullmatch(tag) for tag in tags) or len(set(tags)) != len(tags):
            raise ValueError("explicit unique subset tags required")
        nonempty(item.get("license"), "license", 128)
        nonempty(item.get("source_revision"), "source revision", 200)
        url = urlsplit(nonempty(item.get("source_url"), "source URL", 2000))
        if url.scheme != "https" or not url.hostname or url.username or url.password:
            raise ValueError("source URL must be public HTTPS provenance")
        if item.get("meeting_id") is not None and not ID.fullmatch(nonempty(item["meeting_id"], "meeting id", 80)):
            raise ValueError("invalid meeting identity")
        fixture = dict(item, terms=terms, audio_path=audio, duration_ms=wav_duration(audio))
        fixtures.append(fixture)
    return {"schema_version": 1, "manifest_sha256": hashlib.sha256(raw).hexdigest(), "fixtures": fixtures}


def parse_response(raw, output_format, duration_ms, expected_audio_sha256=None, expected_model=None):
    if not isinstance(raw, bytes) or len(raw) > MAX_RESPONSE_BYTES:
        raise ValueError("adapter output exceeds byte limit")
    doc = strict_json(raw.decode("utf-8"))
    if not isinstance(doc, dict):
        raise ValueError("adapter must return one JSON object")
    if output_format == "fotw-whisper-smoke-v1":
        texts = doc.get("transcripts")
        if type(doc.get("legs")) is not int or doc["legs"] != 1 or not isinstance(texts, list) or len(texts) != 1 or not isinstance(texts[0], str):
            raise ValueError("expected one production Whisper leg")
        nonempty(doc.get("model"), "reported engine model")
        if abs(finite(doc.get("audio_seconds_per_leg")) * 1000 - duration_ms) > 1:
            raise ValueError("adapter transcribed a different audio duration")
        return {"text": texts[0], "segments": None, "timestamps_supported": False,
                "reported_load_ms": finite(doc.get("load_seconds")) * 1000,
                "reported_inference_ms": finite(doc.get("inference_seconds")) * 1000,
                "reported_total_ms": finite(doc.get("total_seconds")) * 1000}
    if output_format != "asr-eval-v1" or type(doc.get("schema_version")) is not int or doc["schema_version"] != 1 or not isinstance(doc.get("text"), str):
        raise ValueError("missing transcript or unsupported adapter schema")
    if expected_audio_sha256 is not None and doc.get("audio_sha256") != expected_audio_sha256:
        raise ValueError("adapter transcribed a different source")
    if expected_model is not None and doc.get("model_id") != expected_model:
        raise ValueError("adapter reported a different model")
    segments = doc.get("segments")
    if segments is not None:
        if not isinstance(segments, list) or len(segments) > 10000 or doc.get("timestamp_unit") != "ms":
            raise ValueError("segments require explicit millisecond timestamps")
        previous = 0
        for segment in segments:
            if not isinstance(segment, dict) or not isinstance(segment.get("text"), str):
                raise ValueError("invalid segment text")
            start, end = segment.get("start_ms"), segment.get("end_ms")
            if type(start) is not int or type(end) is not int or not previous <= start <= end <= duration_ms:
                raise ValueError("invalid or out-of-recording timestamp")
            previous = start
        if tokenize(" ".join(s["text"] for s in segments)) != tokenize(doc["text"]):
            raise ValueError("segment text does not match raw transcript")
    elif doc.get("timestamp_unit") is not None:
        raise ValueError("timestamps claimed without segments")
    return {"text": doc["text"], "segments": segments, "timestamps_supported": segments is not None}


def owned_command(argv, timeout_seconds, max_bytes=MAX_RESPONSE_BYTES, extra_env=None):
    import os
    import signal
    import subprocess
    import tempfile
    import threading
    import time

    finite(timeout_seconds)
    if not 0 < timeout_seconds <= 3600 or type(max_bytes) is not int or not 0 < max_bytes <= MAX_RESPONSE_BYTES:
        raise ValueError("invalid execution limits")
    if not isinstance(argv, list) or not argv or any(not isinstance(arg, str) or not arg or "\0" in arg for arg in argv):
        raise ValueError("expected an explicit argv list")
    if extra_env is not None and (not isinstance(extra_env, dict) or any(not isinstance(k, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", k) or not isinstance(v, str) or "\0" in v for k, v in extra_env.items())):
        raise ValueError("invalid explicitly supplied environment")
    if os.name != "posix":
        return {"status": "unsupported_platform", "stdout": b"", "elapsed_ms": None,
                "measurement": "fresh_process_wall", "returncode": None, "stderr_bytes": 0}
    with tempfile.TemporaryDirectory(prefix="speech-eval-child-") as temporary:
        # Local helpers inherit no credentials, HOME configuration or provider
        # variables. A caller may explicitly supply cloud credentials only after
        # its separate opt-in gate; values never enter the report or diagnostics.
        environment = {"PATH": os.environ.get("PATH", os.defpath),
                       "LANG": os.environ.get("LANG", "C"), "TMPDIR": temporary + os.sep,
                       "PYTHONDONTWRITEBYTECODE": "1"}
        environment.update(extra_env or {})
        started = time.monotonic()
        result = {"status": "ok", "stdout": b"", "elapsed_ms": None,
                  "measurement": "fresh_process_wall", "returncode": None, "stderr_bytes": 0}
        try:
            process = subprocess.Popen(argv, cwd=temporary, env=environment,
                                       stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, start_new_session=True)
        except OSError as error:
            result["status"] = "unavailable" if isinstance(error, FileNotFoundError) else "spawn_failed"
            result["elapsed_ms"] = (time.monotonic() - started) * 1000
            return result
        lock = threading.Lock()
        overflow, io_error = threading.Event(), threading.Event()
        output = bytearray()
        counts = {"total": 0, "stderr": 0}
        def read(pipe, is_stdout):
            try:
                while True:
                    chunk = pipe.read(4096)
                    if not chunk:
                        return
                    with lock:
                        remaining = max(0, max_bytes - counts["total"])
                        counts["total"] += len(chunk)
                        if is_stdout:
                            output.extend(chunk[:remaining])
                        else:
                            counts["stderr"] += len(chunk)
                        if counts["total"] > max_bytes:
                            overflow.set()
                            return
            except OSError:
                io_error.set()
        readers = [threading.Thread(target=read, args=(process.stdout, True), daemon=True),
                   threading.Thread(target=read, args=(process.stderr, False), daemon=True)]
        try:
            for reader in readers:
                reader.start()
            while process.poll() is None or any(reader.is_alive() for reader in readers):
                if overflow.is_set() or io_error.is_set():
                    result["status"] = "output_limit" if overflow.is_set() else "io_error"
                    break
                if time.monotonic() - started >= timeout_seconds:
                    result["status"] = "timeout"
                    break
                time.sleep(.005)
        finally:
            # This session/process group was created by this invocation. Even a
            # successful parent must not leave an owned helper/server behind.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                result["status"] = "cleanup_incomplete"
            for reader in readers:
                if reader.ident is not None:
                    reader.join(timeout=1)
            if any(reader.is_alive() for reader in readers):
                result["status"] = "cleanup_incomplete"
            else:
                process.stdout.close()
                process.stderr.close()
        if overflow.is_set() and result["status"] != "cleanup_incomplete":
            result["status"] = "output_limit"
        elif result["status"] == "ok" and process.returncode != 0:
            result["status"] = "failed"
        result.update(stdout=bytes(output), stderr_bytes=counts["stderr"],
                      returncode=process.returncode,
                      elapsed_ms=(time.monotonic() - started) * 1000)
        return result
