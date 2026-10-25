# Shared vocabulary schema

`fotw_stt::vocabulary::Profile` owns the version-1 JSON schema, validation and
pure snippet matching. It is extracted from Murmur's schema at
`9c6723f07e6996dd0aeca87919211cc5519cc7cc`; copyright/license are retained in
NOTICE. The crate has no profile file path, writer, model download or credentials.
Murmur's storage/editor policy stays in Murmur.

The wire fields stay `version`, `language`, `recognition_hints` and `snippets`.
Version is required and must be 1; legacy files without a language default to
`en`. Unknown fields, including credentials or arbitrary replacement rules, are
rejected. The maximum input and pretty-serialized profile size is 256 KiB.
Recognition hints are limited to 64 unique terms, 128 UTF-8 bytes each and 500
UTF-8 bytes total including separators. These are conservative application
budgets, not a claim about any provider's tokenizer. At most 128 snippets are
allowed, with names up to 80 UTF-8 bytes and literal text up to 16 KiB.

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
not recursively expanded. Recognition hints never cause deterministic replacement.
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
