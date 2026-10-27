import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("speech_eval", Path(__file__).parents[1] / "speech_eval.py")
eval = importlib.util.module_from_spec(spec)
spec.loader.exec_module(eval)


class ScoringTests(unittest.TestCase):
    def test_normalization_preserves_technical_tokens_numbers_and_accents(self):
        self.assertEqual(eval.tokenize("C++ C# HTTP/2 café CAFE\u0301 -12.5 +2 $4 1,000 20%"),
                         ["c++", "c#", "http/2", "café", "café", "-12.5", "+2", "$4", "1,000", "20%"])
        self.assertNotEqual(eval.tokenize("twenty one"), eval.tokenize("21"))

    def test_casefold_and_combining_marks_do_not_split_names_or_drop_accents(self):
        self.assertEqual(eval.tokenize("İ नमस्ते"), ["i\u0307", "नमस्ते"])

    def test_known_substitution_deletion_insertion_and_weighted_errors(self):
        result = eval.score("one two three", "one four")
        self.assertEqual((result["substitutions"], result["deletions"], result["insertions"]), (1, 1, 0))
        self.assertAlmostEqual(result["wer"], 2 / 3)
        self.assertEqual(eval.score("one", "one two three")["wer"], 2.0)

    def test_empty_speech_is_not_missing_output_or_perfect_silence_accuracy(self):
        silent = eval.score("", "imagined words")
        self.assertIsNone(silent["wer"])
        self.assertEqual(silent["hallucinated_words"], 2)
        self.assertEqual(eval.score("hello", "")["deletions"], 1)
        for invalid in [None, {}, 4]:
            with self.assertRaises(ValueError):
                eval.score("hello", invalid)

    def test_terms_count_reference_occurrences_not_substrings_or_duplicates(self):
        result = eval.score("Nova 3 uses Nova 3 with Rust", "Nova 3 with rusty", ["Nova 3", "Rust"])
        self.assertEqual(result["terms_expected"], 3)
        self.assertEqual(result["terms_recalled"], 1)
        self.assertAlmostEqual(result["term_recall"], 1 / 3)
        for terms in [["absent"], ["Rust", "RUST"], [""]]:
            with self.assertRaises(ValueError):
                eval.score("Rust", "Rust", terms)

    def test_numeric_errors_include_sign_decimal_and_unexpected_numbers(self):
        result = eval.score("send -12.5 to 20 accounts", "send 12.5 to 21 accounts and 30")
        self.assertEqual(result["numeric_reference_tokens"], 2)
        self.assertEqual(result["numeric_errors"], 3)
        self.assertEqual(eval.score("twenty", "20")["numeric_errors"], 1)

    def test_latency_uses_documented_nearest_rank_and_rejects_invalid_values(self):
        result = eval.latency_summary(list(range(1, 101)))
        self.assertEqual(result, {"count": 100, "p50_ms": 50, "p95_ms": 95, "max_ms": 100})
        self.assertEqual(eval.latency_summary([]), {"count": 0, "p50_ms": None, "p95_ms": None, "max_ms": None})
        for values in [[-1], [float("nan")], [float("inf")], [True], ["10"]]:
            with self.assertRaises(ValueError):
                eval.latency_summary(values)


if __name__ == "__main__":
    unittest.main()
