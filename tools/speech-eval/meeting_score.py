"""Meeting WER and DER (stub)."""
import json
import sys


def combined_text(hypothesis):
    return ""


def meeting_wer(reference, hypothesis):
    zero = {"reference_words": 0, "substitutions": 0, "deletions": 0, "insertions": 0, "errors": 0, "wer": None}
    return {"combined": zero, "channels": {"mic": zero, "system": zero}}


def optimal_mapping(refs, hyps, overlap):
    return {}, 0


def diarization_error_rate(reference, hypothesis, collar_ms=250, uem=None):
    return {"der": None, "missed_ms": 0, "false_alarm_ms": 0, "confusion_ms": 0, "total_ms": 0, "scored_ms": 0,
            "mapping": {}, "overlap_scored": False}


if __name__ == "__main__":
    print(json.dumps({}))
