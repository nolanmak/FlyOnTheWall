# Recording legally: what you have to do, and what the app does for you

**This is not legal advice.** It is an engineering document explaining what the
app does and why. Recording law is jurisdiction-specific, contested in exactly
the places that matter for AI notetakers, and currently being redrawn by live
litigation. If you are recording client calls commercially, or recording
employees, get a lawyer to look at your specific situation once. It is cheap
relative to the downside.

The downside is not theoretical. California Penal Code § 637.2(a) sets
**statutory damages at $5,000 per violation**, and in a meeting the natural
unit of "violation" is *per recorded participant*. A weekly ten-person call
recorded for a year is not a small number. Federal ECPA adds criminal exposure.

---

## 1. The one rule that decides everything

**Consent is owed to everyone whose voice is captured, under the law of
wherever each of them is sitting — and when those laws differ, the strictest
one governs.**

Everything else is detail. A meeting between someone in Texas (one-party
consent) and someone in California (all-party consent) is an all-party meeting,
because the California participant is protected by California law regardless of
where you are. You do not get to pick the friendlier jurisdiction, and "I was
in a one-party state" is not a defence for recording the person who wasn't.

### One-party vs all-party

- **One-party consent** — you may record a conversation you are part of,
  because you are the consenting party. Federal law and most US states.
- **All-party consent** — every participant must consent. Recording without it
  is a tort and in several states a crime.

The all-party states usually listed are **California, Connecticut, Delaware,
Florida, Illinois, Maryland, Massachusetts, Michigan, Montana, Nevada, New
Hampshire, Oregon, Pennsylvania and Washington** — but treat any specific count
with suspicion. Published lists disagree, sometimes internally: one source
consulted for this document said "these 12 all-party states" and then named
fourteen. Several are genuinely contested rather than merely miscounted —
Michigan's statute reads all-party but courts have held that a *participant*
may record; Nevada differs between in-person and telephonic; Connecticut splits
civil and criminal treatment.

**This is why the app ships statute citations rather than a colour-coded map.**
`crates/fotwd/data/jurisdictions.json` carries 64 jurisdictions, each with the
actual statute and a link, so you can read the law rather than trust our
summary of it. If our summary is wrong, the citation is still right.

### Outside the US

The US consent framing does not travel. Under **GDPR** the question is not "did
they consent" but "do you have a lawful basis under Article 6, chosen and
documented *before* you record."

The counterintuitive part, and the one people get wrong: **consent is usually
the wrong basis in a workplace.** The employer–employee power imbalance makes
consent hard to call freely given, so it is frequently invalid. The normal
basis for internal business meetings is **legitimate interest**, which requires
a balancing test you should write down and keep. On top of the basis you still
owe transparency (say what you record, why, how long you keep it, how to
object), security, and deletion when it is no longer needed. A transcript of an
identifiable person's voice is personal data and the whole framework applies.

---

## 2. What the litigation is actually about

Two live US cases shape the design of this app, and both are worth
understanding because **neither is about recording being illegal.** They are
about *how the product was built*.

- **Chamberlain v. Granola, Inc.**, No. 3:26-cv-07926-EMC (N.D. Cal., filed
  2026-07-30) — pleads CIPA §§ 631/632, § 637.2(a), CDAFA, UCL and ECPA. The
  complaint quotes Granola's own marketing that other participants *"won't know
  it's there."*
- **In re Otter.AI Privacy Litigation**, lead case 5:25-cv-06911 (N.D. Cal.) —
  motion to dismiss heard 2026-05-20. Plaintiffs allege the bot auto-joined via
  calendar integration, that non-users had no notice, that transcripts were
  retained indefinitely, and that they were used to train models without
  disclosure.

Read those allegations as a design specification and you get four rules:

| What was alleged | What the app does instead |
|---|---|
| The bot joined and recorded automatically | Detection **arms**; a human starts. `StartOrigin` has five variants and every one is a person — there is no `Automatic`, so an unstarted recording is unrepresentable in the type system. |
| Other participants had no notice | The Disclosure Kit, and the disclosure record described below. |
| Transcripts retained indefinitely | Retention engine with an explicit budget and eviction. |
| Content used to train models | BYO key, local-only storage, and an egress allowlist that permits exactly the provider you configured. Nothing else can leave. |

The Otter case carries the lesson that matters most for us: it attacks Otter
for **outsourcing consent to customers through the terms of service instead of
building consent mechanisms into the product.** A ToS clause saying "you
promise you got consent" moves paperwork, not risk, and it does nothing for the
person on the other end of the call. That is the thing this app must not do.

---

## 3. The process to actually follow

**Before the meeting.** Put a line in the calendar invite. This is the single
highest-leverage step: it gives notice *before* anyone speaks, it reaches people
who join late, and it is timestamped by someone else's server.

**At the start of every meeting.** Say it out loud, every time, even with people
you record weekly. Two sentences. Then pause — a disclosure nobody had a chance
to respond to is notice, not consent.

**In the chat.** Paste the notice as well as saying it. It reaches latecomers
and leaves an artifact.

**If anyone objects.** Stop. Not "stop next time" — stop, and delete what you
have. One person's objection ends it for the whole call, because you cannot
selectively not-record one participant's audio out of a mixed stream.

**Afterwards.** Keep the record of what you disclosed and when. If it is ever
questioned, the contemporaneous record is the evidence; a memory of having
mentioned it is not.

Run `fotwd disclose` for copy you can paste into all three places.

### Two situations that need a lawyer, not a checklist

- **Recording employees**, anywhere, and especially in the EU/UK. Add works
  councils in Germany and the Netherlands.
- **Regulated conversations** — healthcare (HIPAA), legal privilege, financial
  advice, education (FERPA), or anything involving minors. The recording may be
  lawful and the *transcript* still a compliance problem.

---

## 4. What the app builds in, and what is still missing

Built and verified:

- **64 jurisdictions with statutes and citations**, plus escalation from your
  home jurisdiction, attendee email domains, and the meeting's timezone.
- **Detection arms but never starts.** Enforced by the type system, and by an
  append-only audit log written *before* capture begins.
- **A non-dismissable recording indicator** — level 25 so it survives a
  full-screened meeting window, and it does not hide when you click away.
- **The Disclosure Kit** — chat notice, calendar blurb, verbal script, and a
  pre-filled consent email.
- **Local-only by construction**, with an exact-host egress allowlist.

**The gap, stated plainly.** The `meetings.disclosed` column exists, is
exported, and has a builder — and **nothing in the codebase ever sets it to
true.** The app hands you the words and then does not record that you said
them. That is CON-04, and it is the difference between a tool that helps you
comply and a tool that can show you did. Given that the central allegation in
the Otter case is precisely "no notice to participants," the record of notice
is not bookkeeping — it is the artifact that answers the allegation.

What that needs, concretely:

1. **A disclosure ledger per meeting** — what was disclosed, by which channel
   (calendar / chat / verbal / email), when, and by whom. Append-only, exported
   with the meeting, because evidence you can edit afterwards is not evidence.
2. **A pre-flight gate in all-party jurisdictions.** The escalation is already
   computed; Start should require acknowledging it, and that acknowledgement
   should be what sets `disclosed`.
3. **First-class objection handling.** One action that stops recording *and*
   deletes, because the correct response to an objection is not a menu.
4. **Retention that is a promise, not a default.** "How long do you keep it"
   is a question you have to answer under GDPR, so the answer should be
   configured up front and enforced.

Point 1 is the honest one to be self-critical about: the current design gives
the user the script and then quietly relies on them to remember. That is a
smaller version of the same "outsource it to the customer" move the Otter
complaint attacks.

---

## Sources

- [AI meeting recording laws by state (2026)](https://www.recordinglaw.com/us-laws/ai-meeting-recording-laws/)
- [Otter.ai wiretap lawsuit explained](https://www.recordinglaw.com/news/otter-ai-wiretap-lawsuit-explained/)
- [In re Otter.AI Privacy Litigation — case status](https://ailawsuittracker.com/cases/in-re-otter-ai-privacy-litigation-5-25-cv-06911/)
- [The legality of AI-powered recording and transcription — Reed Smith](https://www.reedsmith.com/our-insights/blogs/employment-law-watch/102ls2n/the-legality-of-ai-powered-recording-and-transcription/)
- [Recording and transcribing internal meetings — global employer guidance](https://igloballaw.com/news-and-events/employment-law/global-recording-and-transcribing-internal-meetings-practical-global-guidance-for-employers/)
- [GDPR meeting recording compliance](https://workgpt.com/en/faq/gdpr-meeting-recording)
- [Chamberlain v. Granola coverage — Computerworld](https://www.computerworld.com/article/4206255/granola-lawsuit-raises-concerns-over-ai-note-taking-app-privacy.html)
