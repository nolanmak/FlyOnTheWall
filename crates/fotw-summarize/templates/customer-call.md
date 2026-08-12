---
name: Customer call
description: A call with a customer or prospect — needs, objections, next steps.
default_for:
  - "*customer*"
  - "*demo*"
  - "*discovery*"
sections:
  - heading: Who was on the call
    guidance: Names and, where they said them, roles and companies. Never guess an affiliation.
    required: true
  - heading: What they need
    guidance: The problem in the customer's own words, quoted where the phrasing matters.
    required: true
  - heading: Objections and risks
    guidance: What they pushed back on, including price, timing and incumbents.
  - heading: Commitments made
    guidance: Anything promised to the customer, by whom, and by when if a date was said.
  - heading: Next steps
    guidance: Concrete follow-ups with owners.
extraction:
  action_items: true
  decisions: true
  open_questions: true
  follow_ups: true
---

What a customer said about their own situation is the most valuable thing in
this transcript and the easiest to paraphrase away. Quote them.
