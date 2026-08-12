---
name: Standup
description: Daily standup — yesterday, today, blockers, per person.
default_for:
  - "*standup*"
  - "*stand-up*"
  - "daily *"
sections:
  - heading: Per person
    guidance: One block per speaker, in the order they spoke. Under each, what they finished, what they are on next, and what is in their way. Omit a person who did not speak.
    required: true
  - heading: Blockers
    guidance: Only things someone actually called a blocker, with who is blocked and on whom.
  - heading: Action items
    guidance: Carried over from the blockers and the asks. Owner exactly as named, or none.
extraction:
  action_items: true
  decisions: false
  open_questions: true
  follow_ups: true
effort_hint: low
---

A standup is short and the notes should be shorter. Do not editorialise, do not
merge two people's updates, and do not turn "I'll look at it" into a commitment
with a date attached to it.
