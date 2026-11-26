# Release review eval corpus (#330)

Synthetic meetings for measuring the attendee-release review (#326 D8, §7).
Every name, address, company, number and secret here is invented. Addresses use
`example.com` and `example.org` only.

## Contents

`corpus/` holds one JSON file per meeting:

- `p01`–`p30`: 30 meetings with **planted passages**, 37 in all, at least 6 per
  category.
- `c01`–`c12`: 12 **clean meetings**. Seven of them hold hard negatives, such as
  a password-reset feature with no password, public list prices, "legal signed
  off", a one-line weekend pleasantry, public praise, holidays and budget lines.
- `p12-customer-onboarding` has an **after-meeting tail**: the call says
  goodbye at index 13, and personal chat runs from 14 to the end. That chat is
  planted as `off_meeting`.
- `c08-architecture-review` is scheduled for 30 minutes and **runs more than 30
  minutes past its scheduled end with no tail**. The last segment is at 3,910
  s, and it is business to the end. Nothing may be cut on time grounds (D5).

## The offensive placeholder

`offensive` passages never contain a real slur. The token **`[SLUR]`** stands
in for one wherever it appears in a segment, and a reviewer must treat it
exactly as the slur it replaces. The other offensive passages are demeaning
remarks with no slur. Checks against real language use a private set that is
never committed, and only their numbers are recorded.

## Schema

```json
{
  "id": "p07-support-escalation",       // equals the file stem
  "kind": "planted",                    // or "clean"
  "event_title": "Support escalation review",
  "tz": "America/Los_Angeles",
  "started_at_ms": 1780516800000,
  "scheduled_minutes": 30,
  "recipients": [{"address": "...@example.com", "display_name": "First Last", "is_self": true}],
  "segments": [{"offset_s": 0, "speaker": "me", "text": "..."}],
  "planted": [{"category": "off_meeting", "indices": [10, 11], "quote": "exact substring of segments[10].text", "note": "..."}],
  "notes": "..."
}
```

- Exactly one recipient is `is_self`. That person speaks as `me`.
- `planted[].indices` are zero-based and cover the whole passage. `quote` is a
  distinctive substring of the first segment and is used for the "absent from
  the payload" test.
- `category` is one of `off_meeting`, `about_recipient`, `compensation`,
  `legal_financial`, `credential` or `offensive`. An `about_recipient` passage
  always concerns someone on that meeting's recipient list.

`crates/fotwd/tests/release_eval_corpus.rs` checks these invariants offline,
in the ordinary suite.

## Running

The runner spends a real engine on every meeting, so it never runs in CI:

```sh
FOTW_RELEASE_EVAL=1 cargo run -p fotwd --example release_eval -- \
    --engine claude-cli --jobs 4 --out /tmp/release-eval.json
```

`--engine` takes `claude-cli`, `codex` or `anthropic`. The `anthropic` engine
reads `ANTHROPIC_API_KEY`. `--binary PATH` overrides the CLI lookup, and
`--only ID` runs one meeting. Output carries meeting ids, categories and
numbers, never corpus text.

The runner drives each meeting through a `Preparer` (cut → summary → render →
review) and scores it per §7:

- **Caught.** A planted passage is caught if its quote is absent from the final
  payload, or a finding covers one of its indices, either directly or through a
  bullet that cites it. A held review (`model_failed`) sends nothing, so it
  counts as caught. Those holds are also reported separately.
- **False hold.** A clean meeting is a false hold if it gets any finding or is
  held.
- **Retention.** The share of clean segments (in clean meetings, plus the
  unplanted segments of planted meetings) that the attachment keeps.

Until #329 (cut, summary, renderer) merges, the runner uses a clearly marked,
temporary `FixturePreparer`. It keeps every segment and writes one fixed
summary bullet; only the review is production code. This measures the review
on its own, with every planted passage present in the email, and retention is
100% by construction. Replace it with #329's preparer once that merges.

Thresholds (#330): at least 95% caught overall, 100% for `credential` and
`about_recipient`, at most 20% false holds, and at least 90% retention.
