---
name: Design review
description: A design or architecture review — the proposal, the objections, what was settled.
default_for:
  - "*design review*"
  - "*architecture*"
  - "*rfc*"
  - "*tech spec*"
sections:
  - heading: Proposal
    guidance: What was being reviewed, in one paragraph, as the author described it.
    required: true
  - heading: Alternatives considered
    guidance: Options discussed and why each was or was not taken. Record the reason given, not one you supply.
  - heading: Objections
    guidance: Each concern raised, who raised it, and whether it was answered.
  - heading: Decisions
    guidance: Only what was actually settled in the room. "We'll think about it" is not a decision.
  - heading: Follow-ups
    guidance: Work the review created, with owners.
extraction:
  action_items: true
  decisions: true
  open_questions: true
  follow_ups: true
effort_hint: high
---

The valuable part of a design review is the disagreement. Preserve it: an
objection that was raised and not resolved must not be smoothed into consensus.
