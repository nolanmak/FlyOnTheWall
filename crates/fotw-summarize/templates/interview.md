---
name: Interview
description: A candidate interview — signal against the questions asked, nothing more.
default_for:
  - "*interview*"
  - "*screen*"
sections:
  - heading: Questions and answers
    guidance: Each question asked, and a faithful account of the answer. This is the section that matters.
    required: true
  - heading: Evidence
    guidance: Specific things the candidate described doing. Attribute each to what they said.
  - heading: Candidate questions
    guidance: What they asked, and what they were told.
  - heading: Open follow-ups
    guidance: Anything the panel said it would check or ask later.
extraction:
  action_items: true
  decisions: false
  open_questions: true
  follow_ups: true
effort_hint: high
---

Do not score the candidate, do not recommend a decision, and do not infer
anything about a protected characteristic from a name, an accent or a school.
Report what was asked and what was answered; the humans decide.
