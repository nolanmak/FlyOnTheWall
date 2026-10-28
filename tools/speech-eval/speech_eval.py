"""Paired ASR scoring and explicit, owned fresh-process experiments. No downloads."""
import math
import re
import unicodedata

NORMALIZATION = "nfc-casefold-technical-numeric-v2"
NUMBER = r"(?:[$€£]?[+-]?|[+-]?[$€£]?)\d+(?:[.,]\d+)*(?:%)?"
TOKEN = re.compile(NUMBER + r"(?!\w|[._:/’'-]+\w)|\w+(?:[._:/’'-]+\w+)*(?:\+\+(?:\d+)?|#)?", re.UNICODE)
MAX_ALIGNMENT_CELLS = 12_000_000


def tokenize(text):
    if not isinstance(text, str):
        raise ValueError("transcript must be a string; missing output is not silence")
    normalized = unicodedata.normalize("NFC", text).casefold()
    marks = "".join(sorted({c for c in normalized if unicodedata.category(c).startswith("M")}))
    if not marks:
        return TOKEN.findall(normalized)
    word = r"[\w" + re.escape(marks) + "]"
    pattern = NUMBER + r"(?!" + word + r"|[._:/’'-]+" + word + r")|\w" + word + r"*(?:[._:/’'-]+" + word + r"+)*(?:\+\+(?:\d+)?|#)?"
    return re.findall(pattern, normalized)


def edit_counts(reference, hypothesis, limit=None):
    # Full-length meeting scoring passes an explicit, larger limit.
    if len(reference) * len(hypothesis) > (MAX_ALIGNMENT_CELLS if limit is None else limit):
        raise ValueError("scoring alignment exceeds the documented work limit")
    # Cost, substitutions, deletions, insertions. Two rows bound memory.
    row = [(n, 0, 0, n) for n in range(len(hypothesis) + 1)]
    for i, ref in enumerate(reference, 1):
        current = [(i, 0, i, 0)]
        for j, hyp in enumerate(hypothesis, 1):
            cost, subs, deletes, inserts = row[j - 1]
            change = int(ref != hyp)
            diagonal = (cost + change, subs + change, deletes, inserts)
            cost, subs, deletes, inserts = row[j]
            deletion = (cost + 1, subs, deletes + 1, inserts)
            cost, subs, deletes, inserts = current[j - 1]
            insertion = (cost + 1, subs, deletes, inserts + 1)
            # Stable tie order: diagonal, deletion, insertion.
            current.append(min((diagonal, deletion, insertion), key=lambda x: x[0]))
        row = current
    errors, substitutions, deletions, insertions = row[-1]
    return {"reference_words": len(reference), "hypothesis_words": len(hypothesis),
            "matches": len(reference) - substitutions - deletions,
            "substitutions": substitutions, "deletions": deletions,
            "insertions": insertions, "errors": errors,
            "wer": errors / len(reference) if reference else None}


def occurrences(words, phrase):
    return sum(words[i:i + len(phrase)] == phrase for i in range(len(words) - len(phrase) + 1))


def score(reference, hypothesis, terms=()):
    ref, hyp = tokenize(reference), tokenize(hypothesis)
    result = edit_counts(ref, hyp)
    result["hallucinated_words"] = len(hyp) if not ref else 0
    if not isinstance(terms, (list, tuple)):
        raise ValueError("terms must be an explicit list")
    expected = recalled = 0
    seen = set()
    for term in terms:
        phrase = tokenize(term)
        key = tuple(phrase)
        count = occurrences(ref, phrase) if phrase else 0
        if not count or key in seen:
            raise ValueError("terms must be distinct nonempty phrases present in the reference")
        seen.add(key)
        expected += count
        recalled += min(count, occurrences(hyp, phrase))
    result.update(terms_expected=expected, terms_recalled=recalled,
                  term_recall=recalled / expected if expected else None)
    numeric = edit_counts([t for t in ref if re.fullmatch(NUMBER, t)],
                          [t for t in hyp if re.fullmatch(NUMBER, t)])
    result.update(numeric_reference_tokens=numeric["reference_words"], numeric_errors=numeric["errors"])
    return result


def latency_summary(values):
    if any(isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v) or v < 0 for v in values):
        raise ValueError("latencies must be finite nonnegative milliseconds")
    ordered = sorted(values)
    def percentile(p):
        return ordered[math.ceil(p * len(ordered)) - 1] if ordered else None
    return {"count": len(ordered), "p50_ms": percentile(.50),
            "p95_ms": percentile(.95), "max_ms": ordered[-1] if ordered else None}
