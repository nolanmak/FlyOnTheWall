"""Paired, pinned ASR experiments; no downloads or implicit cloud access."""
import hashlib
import json
import os
import platform
from pathlib import Path
import re
import shutil
import struct
import sys
import tempfile
import time
import wave

import benchmark as protocol
from speech_eval import NORMALIZATION, latency_summary, score

HASH = re.compile(r"[0-9a-f]{64}\Z")
UNMEASURED = ["warm_release_to_result", "process_tree_peak_memory", "memory_pressure_swap",
              "30_minute_workload", "dual_app_contention", "meeting_streaming_backlog", "physical_app_acceptance",
              "thermal_desktop_load", "license_review"]


def asset_digest(path):
    """File SHA-256 or versioned, sorted directory-tree fingerprint, without symlinks."""
    path = Path(path)
    if path.is_symlink():
        raise ValueError("pinned asset cannot be a symlink")
    if path.is_file():
        if path.stat().st_size > 16 * 1024**3:
            raise ValueError("asset exceeds fingerprint limit")
        return protocol.sha256_file(path)
    if not path.is_dir():
        raise FileNotFoundError("asset unavailable")
    entries = []
    size = 0
    for entry in sorted(path.rglob("*")):
        if entry.is_symlink():
            raise ValueError("model bundle cannot contain symlinks")
        if entry.is_dir():
            continue
        if not entry.is_file():
            raise ValueError("model bundle contains a non-file")
        size += entry.stat().st_size
        if len(entries) >= 10000 or size > 16 * 1024**3:
            raise ValueError("model bundle exceeds fingerprint limits")
        entries.append([entry.relative_to(path).as_posix(), protocol.sha256_file(entry)])
    if not entries:
        raise ValueError("empty model bundle")
    encoded = json.dumps(["speech-eval-tree-v1", entries], separators=(",", ":"), ensure_ascii=True)
    return hashlib.sha256(encoded.encode()).hexdigest()


def load_candidates(path):
    path = Path(path)
    if path.stat().st_size > 131072:
        raise ValueError("candidate manifest too large")
    raw = path.read_bytes()
    doc = protocol.strict_json(raw)
    if not isinstance(doc, dict) or set(doc) != {"schema_version", "candidates"} or type(doc["schema_version"]) is not int or doc["schema_version"] != 1:
        raise ValueError("unsupported candidate manifest")
    if not isinstance(doc["candidates"], list) or not 1 <= len(doc["candidates"]) <= 8:
        raise ValueError("expected 1–8 explicit candidates")
    allowed = {"id", "mode", "format", "executable", "executable_sha256", "runtime_revision",
               "model_id", "model_path", "model_sha256", "argv", "timeout_seconds",
               "credential_env", "use_terms", "unconfigured", "runtime_assets"}
    seen, candidates = set(), []
    for item in doc["candidates"]:
        if not isinstance(item, dict) or set(item) - allowed:
            raise ValueError("unknown candidate field")
        identity = protocol.nonempty(item.get("id"), "candidate id", 80)
        if not protocol.ID.fullmatch(identity) or identity in seen or item.get("mode") not in {"local", "cloud"}:
            raise ValueError("duplicate/invalid candidate id or mode")
        seen.add(identity)
        if item.get("unconfigured") is True:
            if set(item) != {"id", "mode", "unconfigured"}:
                raise ValueError("unconfigured candidate cannot claim artifact pins")
            candidates.append(dict(item))
            continue
        if "unconfigured" in item:
            raise ValueError("unconfigured must be explicitly true or absent")
        if item.get("format") not in {"asr-eval-v1", "fotw-whisper-smoke-v1"}:
            raise ValueError("unsupported adapter format")
        for key in ("executable", "runtime_revision", "model_id"):
            protocol.nonempty(item.get(key), key, 2000 if key == "executable" else 200)
        if not Path(item["executable"]).is_absolute() or not isinstance(item.get("executable_sha256"), str) or not HASH.fullmatch(item["executable_sha256"]):
            raise ValueError("executable requires an absolute path and SHA-256")
        if item["mode"] == "local":
            if "credential_env" in item:
                raise ValueError("local candidates cannot resolve credentials")
            if not isinstance(item.get("model_path"), str) or not Path(item["model_path"]).is_absolute() or not isinstance(item.get("model_sha256"), str) or not HASH.fullmatch(item["model_sha256"]):
                raise ValueError("local model requires an explicit absolute path and fingerprint")
        elif item.get("credential_env") != "DEEPGRAM_API_KEY" or "model_path" in item or "model_sha256" in item:
            raise ValueError("cloud adapter requires explicit Deepgram environment credential policy")
        if type(item.get("use_terms", False)) is not bool:
            raise ValueError("use_terms must be boolean")
        if item["format"] == "fotw-whisper-smoke-v1":
            if item["mode"] != "local" or item.get("use_terms", False) or "argv" in item:
                raise ValueError("production Whisper smoke has fixed local arguments and no keyterm support")
        else:
            argv = item.get("argv")
            if not isinstance(argv, list) or not 1 <= len(argv) <= 32 or any(not isinstance(a, str) or not a or len(a) > 4000 or "\0" in a for a in argv) or not any("{audio_wav}" in a for a in argv):
                raise ValueError("generic adapter needs bounded argv with an explicit audio placeholder")
        assets = item.get("runtime_assets", [])
        if not isinstance(assets, list) or len(assets) > 32:
            raise ValueError("invalid runtime asset list")
        asset_ids = set()
        for asset in assets:
            if not isinstance(asset, dict) or set(asset) != {"id", "path", "sha256"}:
                raise ValueError("invalid runtime asset pin")
            if not isinstance(asset["id"], str) or not protocol.ID.fullmatch(asset["id"]) or asset["id"] in asset_ids:
                raise ValueError("duplicate or invalid runtime asset id")
            asset_ids.add(asset["id"])
            if not isinstance(asset["path"], str) or not Path(asset["path"]).is_absolute() or not isinstance(asset["sha256"], str) or not HASH.fullmatch(asset["sha256"]):
                raise ValueError("runtime assets need absolute paths and fingerprints")
        timeout = protocol.finite(item.get("timeout_seconds", 180))
        if not 0 < timeout <= 3600:
            raise ValueError("invalid per-request timeout")
        candidates.append(dict(item, timeout_seconds=timeout))
    return candidates, hashlib.sha256(raw).hexdigest()


def offline_guard(argv):
    """Local execution is refused where this runner cannot deny networking."""
    sandbox = Path("/usr/bin/sandbox-exec")
    if sys.platform != "darwin" or not sandbox.is_file():
        return None
    return ([str(sandbox), "-p", "(version 1)(allow default)(deny network*)", *argv],
            "macos-sandbox-deny-network")


def snapshot(corpus, folder):
    fixtures = []
    for index, source in enumerate(corpus["fixtures"]):
        destination = folder / (str(index) + ".wav")
        shutil.copyfile(source["audio_path"], destination)
        if protocol.sha256_file(destination) != source["sha256"]:
            raise ValueError("fixture changed while snapshotting")
        floats = destination.with_suffix(".f32le")
        with wave.open(str(destination), "rb") as audio, floats.open("wb") as output:
            while True:
                block = audio.readframes(16000)
                if not block:
                    break
                pcm = struct.unpack("<" + "h" * (len(block) // 2), block)
                output.write(struct.pack("<" + "f" * len(pcm), *(v / 32768.0 for v in pcm)))
        fixtures.append(dict(source, audio_path=destination, float_path=floats,
                             float_sha256=protocol.sha256_file(floats)))
    return fixtures


def input_intact(fixture):
    try:
        return (protocol.sha256_file(fixture["audio_path"]) == fixture["sha256"]
                and protocol.sha256_file(fixture["float_path"]) == fixture["float_sha256"])
    except OSError:
        return False


def arguments(candidate, fixture):
    if candidate["format"] == "fotw-whisper-smoke-v1":
        return [candidate["executable"], candidate["model_path"], str(fixture["float_path"]), "1"]
    values = {"audio_wav": str(fixture["audio_path"]), "audio_f32": str(fixture["float_path"]),
              "model": candidate.get("model_path", candidate["model_id"]),
              "model_id": candidate["model_id"],
              "terms_json": json.dumps(fixture["terms"] if candidate.get("use_terms") else [])}
    # One substitution pass, no shell and no interpretation of literal braces.
    token = re.compile(r"\{(audio_wav|audio_f32|model|model_id|terms_json)\}")
    return [candidate["executable"], *(token.sub(lambda m: values[m[1]], a) for a in candidate["argv"])]


def aggregate(rows):
    total = {key: sum(row["score"][key] for row in rows) for key in (
        "reference_words", "hypothesis_words", "errors", "substitutions", "deletions",
        "insertions", "terms_expected", "terms_recalled", "numeric_reference_tokens",
        "numeric_errors", "hallucinated_words")}
    total["wer"] = total["errors"] / total["reference_words"] if total["reference_words"] else None
    total["term_recall"] = total["terms_recalled"] / total["terms_expected"] if total["terms_expected"] else None
    total["trials"] = len(rows)
    return total


def host_metadata():
    data = {"system": platform.system(), "release": platform.release(), "machine": platform.machine(),
            "python": platform.python_version(), "logical_cpus": os.cpu_count()}
    if sys.platform == "darwin":
        result = protocol.owned_command(["/usr/sbin/sysctl", "-n", "machdep.cpu.brand_string", "hw.memsize", "hw.model", "kern.osproductversion"], 2, max_bytes=2048)
        if result["status"] == "ok":
            parts = result["stdout"].decode("utf-8", errors="replace").splitlines()
            if len(parts) == 4 and parts[1].isdigit():
                data.update(cpu=parts[0], physical_memory_bytes=int(parts[1]), hardware_model=parts[2], macos_version=parts[3])
        data["hardware_probe_status"] = result["status"]
    if hasattr(os, "getloadavg"):
        data["load_average_at_report"] = list(os.getloadavg())
    return data


def corpus_coverage(fixtures):
    kinds = ("human", "synthetic", "silence")
    meetings = {}
    for fixture in fixtures:
        if fixture.get("meeting_id"):
            meetings.setdefault(fixture["meeting_id"], []).append(fixture)
    paired = [identity for identity, tracks in meetings.items()
              if len(tracks) == 2 and {t["track"] for t in tracks} == {"mic", "system"}
              and min(t["duration_ms"] for t in tracks) >= 600000
              and abs(tracks[0]["duration_ms"] - tracks[1]["duration_ms"]) <= 1]
    return {"annotation_claims_only": True,
            "fixtures_by_kind": {k: sum(f["kind"] == k for f in fixtures) for k in kinds},
            "short_utterances_by_kind": {k: sum(f["kind"] == k and f["track"] == "utterance" and f["duration_ms"] <= 120000 for f in fixtures) for k in kinds},
            "ten_minute_paired_meeting_ids": sorted(paired)}


def run(corpus_path, candidate_path, repetitions=1, allow_cloud=False, max_total_seconds=3600,
        executor=protocol.owned_command, guard=offline_guard, credential_reader=os.environ.get):
    if type(repetitions) is not int or not 1 <= repetitions <= 10 or type(allow_cloud) is not bool:
        raise ValueError("invalid repetition count or cloud policy")
    protocol.finite(max_total_seconds)
    if not 0 < max_total_seconds <= 86400:
        raise ValueError("invalid total execution budget")
    corpus = protocol.load_corpus(corpus_path)
    candidates, spec_digest = load_candidates(candidate_path)
    if len(corpus["fixtures"]) * len(candidates) * repetitions > 10000:
        raise ValueError("experiment exceeds 10000 request limit")
    if sum(f["audio_path"].stat().st_size for f in corpus["fixtures"]) > 2 * 1024**3:
        raise ValueError("corpus exceeds 2 GiB snapshot limit")
    states, credentials = {}, {}
    for candidate in candidates:
        identity = candidate["id"]
        if candidate["mode"] == "cloud" and not allow_cloud:
            states[identity] = "skipped_cloud_not_enabled"
            continue
        if candidate.get("unconfigured"):
            states[identity] = "skipped_unconfigured"
            continue
        if candidate["mode"] == "cloud":
            key = credential_reader(candidate["credential_env"])
            if not key:
                states[identity] = "skipped_missing_credential"
                continue
            credentials[identity] = {candidate["credential_env"]: key}
        elif guard([candidate["executable"]]) is None:
            states[identity] = "unsupported_offline_guard"
            continue
        try:
            if asset_digest(candidate["executable"]) != candidate["executable_sha256"] or (
                candidate["mode"] == "local" and asset_digest(candidate["model_path"]) != candidate["model_sha256"]
            ):
                states[identity] = "asset_mismatch"
            if any(asset_digest(a["path"]) != a["sha256"] for a in candidate.get("runtime_assets", [])):
                states[identity] = "asset_mismatch"
        except (OSError, ValueError):
            states[identity] = "asset_unavailable"
    started = time.monotonic()
    rows, compromised = [], False
    with tempfile.TemporaryDirectory(prefix="paired-speech-inputs-") as owned:
        fixtures = snapshot(corpus, Path(owned))
        for fixture_index, fixture in enumerate(fixtures):
            for trial in range(repetitions):
                rotation = (fixture_index * repetitions + trial) % len(candidates)
                ordered = candidates[rotation:] + candidates[:rotation]
                for candidate in ordered:
                    row = {"fixture": fixture["id"], "candidate": candidate["id"], "trial": trial,
                           "kind": fixture["kind"], "tags": fixture["tags"],
                           "duration_ms": fixture["duration_ms"]}
                    rows.append(row)
                    if compromised or not input_intact(fixture):
                        compromised = True
                        row["status"] = "input_changed"
                        continue
                    state = states.get(candidate["id"])
                    if state:
                        row["status"] = state
                        continue
                    remaining = max_total_seconds - (time.monotonic() - started)
                    if remaining <= 0:
                        row["status"] = "experiment_budget_exhausted"
                        continue
                    argv = arguments(candidate, fixture)
                    if candidate["mode"] == "local":
                        guarded = guard(argv)
                        if guarded is None:
                            row["status"] = "unsupported_offline_guard"
                            continue
                        argv, policy = guarded
                    else:
                        policy = "explicit_cloud_opt_in"
                    row["network_policy"] = policy
                    result = executor(argv, min(candidate["timeout_seconds"], remaining),
                                      extra_env=credentials.get(candidate["id"]))
                    row.update(status=result["status"], elapsed_ms=result["elapsed_ms"])
                    if not input_intact(fixture):
                        compromised = True
                        row["status"] = "input_changed"
                        continue
                    if result["status"] != "ok":
                        continue
                    try:
                        protocol.finite(result["elapsed_ms"])
                        if result["measurement"] != "fresh_process_wall":
                            raise ValueError("unknown timing scope")
                        response = protocol.parse_response(result["stdout"], candidate["format"], fixture["duration_ms"],
                            expected_audio_sha256=fixture["sha256"], expected_model=candidate["model_id"])
                        row["fresh_process_real_time_factor"] = result["elapsed_ms"] / fixture["duration_ms"]
                        row["timestamps_supported"] = response["timestamps_supported"]
                        row["reported_timing"] = {k: v for k, v in response.items() if k.startswith("reported_")}
                    except (ValueError, UnicodeError, KeyError, TypeError):
                        row["status"] = "invalid_response"
                        continue
                    try:
                        row["score"] = score(fixture["reference"], response["text"], fixture["terms"])
                    except ValueError:
                        row["status"] = "scoring_work_limit"
    # Never retain transcripts, command arguments, artifact paths or credentials.
    summaries = []
    for candidate in candidates:
        trials = [r for r in rows if r["candidate"] == candidate["id"]]
        good = [r for r in trials if r["status"] == "ok"]
        subsets = sorted({r["kind"] + "/" + tag for r in good for tag in r["tags"]})
        summaries.append({"id": candidate["id"], "mode": candidate["mode"],
            "model_id": candidate.get("model_id"), "runtime_revision": candidate.get("runtime_revision"),
            "executable_sha256": candidate.get("executable_sha256"), "model_sha256": candidate.get("model_sha256"),
            "runtime_assets": [{"id": a["id"], "sha256": a["sha256"]} for a in candidate.get("runtime_assets", [])],
            "use_terms": candidate.get("use_terms", False), "attempted_trials": len(trials),
            "successful_trials": len(good), "successful_trials_only": {
                "all": aggregate(good),
                "by_kind": {kind: aggregate([r for r in good if r["kind"] == kind]) for kind in ("human", "synthetic", "silence")},
                "subsets": {tag: aggregate([
                    r for r in good if tag in {r["kind"] + "/" + t for t in r["tags"]}]) for tag in subsets}},
            "timing": dict(latency_summary([r["elapsed_ms"] for r in good]),
                           measurement="fresh_process_wall", scope="all_successful_trials_mixed_durations",
                           real_time_factor=sum(r["elapsed_ms"] for r in good) / sum(r["duration_ms"] for r in good) if good else None),
            "timing_by_duration_ms": {str(duration): latency_summary([
                r["elapsed_ms"] for r in good if r["duration_ms"] == duration])
                for duration in sorted({r["duration_ms"] for r in good})}})
    complete = len(candidates) >= 2 and bool(rows) and all(r["status"] == "ok" for r in rows)
    return {"schema_version": 1, "normalization": NORMALIZATION,
            "host": host_metadata(), "manifest_coverage": corpus_coverage(corpus["fixtures"]),
            "corpus_manifest_sha256": corpus["manifest_sha256"], "candidate_manifest_sha256": spec_digest,
            "fixtures": [{k: f[k] for k in ("id", "sha256", "kind", "track", "tags", "duration_ms")} for f in corpus["fixtures"]],
            "repetitions": repetitions, "paired_execution_complete": complete,
            "comparison": {c["id"]: c["successful_trials_only"] for c in summaries} if complete else None,
            "candidates": summaries, "rows": rows, "unmeasured": UNMEASURED,
            "release_qualification": "not_established_by_this_runner"}
