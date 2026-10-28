# Shared vocabulary schema

`fotw_stt::vocabulary::Profile` owns the version-1 JSON schema, validation and
pure snippet matching. It is extracted from Murmur's schema at
`9c6723f07e6996dd0aeca87919211cc5519cc7cc`; copyright/license are retained in
NOTICE. The crate has no profile file path, writer, model download or credentials.
Murmur's storage/editor policy stays in Murmur.

The wire fields are `version`, `language`, `recognition_hints`, `snippets` and,
added in place without a version bump, `corrections`. Version is required and
must be 1; legacy files without a language default to `en`, and files without
`corrections` load unchanged. A profile with no corrections serializes without
the field, so older readers still accept it. Unknown fields, including
credentials or free-form rewrite rules, are rejected. The maximum input and pretty-serialized profile size is 256 KiB.
Recognition hints are limited to 64 unique terms, 128 UTF-8 bytes each and 500
UTF-8 bytes total including separators. These are conservative application
budgets, not a claim about any provider's tokenizer. At most 128 snippets are
allowed, with names up to 80 UTF-8 bytes and literal text up to 16 KiB. At
most 512 corrections are allowed; each `word` is one to eight single-spaced
words of up to 128 UTF-8 bytes, unique ignoring case and accents (the
matcher's comparison, so `Jose` and `José` conflict), with an optional
`replacement` of the same size and an optional `matching_threshold` between
0.70 and 0.95 (default 0.85), Muesli's names and range.

`hints_for(capabilities.custom_vocabulary)` distinguishes no request,
unsupported hints and a supported keyterm/keyword/prompt mechanism. Supported
means the capability exists; the adapter still must encode the request. These
mechanisms are not equivalent. A local adapter declaring `None` reports the
limitation and never sends hints to a hosted repair service. Snippets are never
included in recognition hints or automatically converted into prompts.

Snippet expansion requires the entire trimmed utterance to be `insert snippet
NAME`, optionally followed by one `.`, `!` or `?`. Matching uses Unicode
lowercase comparison; it does not normalize Unicode or rewrite arbitrary prose,
code, repeated phrases or word fragments. Expansion text stays literal and is
not recursively expanded. Snippets are applied after local corrections.
The meeting recorder does not start executing spoken snippets by importing this
schema; application code must explicitly opt into deterministic expansion.

The producer-owned `conformance::vocabulary()` runs the shared fixture matrix
for names, numbers, technical terms, Unicode, exact boundaries and repeated
phrases. Ordinary producer unit tests and consuming apps invoke the same
contract; `tests/vocabulary.rs` also checks malformed schemas and limits.
Murmur retains its file import rollback, writer-lock, native editor, immutable
utterance and real loopback request tests. Fixtures prove contract behavior,
not improved model recall; the paired quality study remains in FOTW #118 and
Murmur #9.

## Local corrections

`vocabulary_matcher::VocabularyMatcher` ports Muesli's `CustomWordMatcher`
(MIT, commit `97e455794112126a896cae17f5c829e8ffa820c8`) to correct
**local-engine** text after recognition. An application opts in explicitly and
must not apply it to a provider that already received the recognition hints
(Deepgram keyterms): that text is left as the provider returned it.

Its targets are each `corrections` entry (written as `replacement`, or as
`word` when there is none), then each recognition hint not already listed, as
a correction to itself. At each word the longest phrase wins. A case- and
accent-folded exact match beats a near miss; a near miss needs Jaro–Winkler
similarity at or above the entry's threshold for every word of the phrase.
Whitespace and the punctuation around a phrase are kept, and punctuation
inside a phrase blocks it.

Muesli is built for small dictionaries. So that a 200-entry profile does not
rewrite ordinary prose, a near miss also never replaces a frequent English word
or its inflection (`src/common_words_en.txt`), a word containing a digit, or a
longer word containing the entry; its length must be within one character of
the entry's (two when the threshold is lowered), and its first letter the same
or a sound-alike. Adjacent words that spell a camel-case entry (`web assembly`,
`Fluid Audio`) are joined unless every one of them is a frequent English word:
`linked in`, `I phone`, `power point` and `fly on the wall` stay prose. Add a
multi-word correction (`fly on the wall` → `FlyOnTheWall`) to rewrite such a
phrase deliberately. `tests/fixtures/vocabulary-corrections-v1.json`
pins every rule, and `conformance::vocabulary()` runs it for consumers.

Measured for Murmur #102 with whisper large-v3-turbo on the #118 corpus: term
recall on the 12 synthetic technical-term clips rose from 10/14 to 13/14 with a
profile of their terms, and WER fell from 15.4% to 11.1%; 42 LibriSpeech clips
were unchanged. (`Fly on the Wall` stays apart because all four words are
common; a `fly on the wall` correction would recover it.) With 200 entries (those terms plus 186 other product and
people names), recall and synthetic WER were the same, and 4 of 1,308
LibriSpeech words were wrongly replaced (WER 3.4% → 3.7%). The
`vocabulary_correct` example reproduces the timing; every utterance stayed
under 1 ms at p95.

### Constructing profiles in Rust

`Profile` gains fields additively within version 1 (`corrections` was the
first). Consumers that build one in code should write
`Profile { recognition_hints, ..Default::default() }` rather than listing every
field, so a new optional field does not break their build.

