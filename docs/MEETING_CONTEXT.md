# Attendee context and name repair

## Implemented: explicit corrections

A saved meeting document exposes **Review or edit → Remember name correction**.
The user supplies the transcribed name and verified spelling. The mapping is
scoped to that meeting, stored in its versioned document, and travels in library
archives. It corrects the current summary by adding a version and updates the
saved brief. Regenerated summaries and documents honor the stored mapping.
Browser Markdown/PDF sharing copies mark corrected source names in brackets.
Original audio, stored transcript text, and speaker labels remain intact.
GitHub auto export syncs the new summary and saved brief revisions.

Matching uses whole names, prefers the longest match, and never chains replacements.
These are explicit user corrections, not fuzzy guesses. A mapping is not evidence
that a particular voice belongs to that person. This first UI requires a saved
meeting document. Corrections are not yet shared across meetings or accounts.

## Next: calendar/email evidence

The shipped meeting detector still uses `NoCalendar`. Calendar and email account
connections are not implemented by this change. Connecting an account alone will
not make transcription context-aware; the following flow is needed:

1. Link a recording to a calendar event using its actual start/end, conferencing
   URL, organizer, and event ID. Ask when concurrent events make the match ambiguous.
2. Retrieve attendee display names, addresses, organizer, event title, and relevant
   description using read-only calendar access. An invite is candidate identity
   evidence, not proof of actual attendance or a speaker label.
3. If needed, retrieve relevant email threads involving those attendees and the
   event. Use thread IDs, sender identity and signatures as evidence; do not ingest
   the whole mailbox. Email bodies and event descriptions are untrusted data and
   cannot issue tool instructions or authorize sharing.
4. Generate candidate name repairs from phonetic similarity and local context.
   Require corroboration such as a matching organization, role or introduction.
   A single similar first name is insufficient. Contradictory or ambiguous evidence
   becomes a review item; it must not silently relabel every occurrence.
5. Resolve precedence as explicit user correction > stable contact/event identity >
   corroborated email evidence > transcript spelling. Preserve evidence references,
   confidence, original text, and corrected text with each proposed/applied repair.
6. Maintain a user-editable identity record keyed by account and contact identity,
   not a global replacement of common first names. Let users undo a correction.
7. Refresh derived summaries and sharing documents after an accepted repair;
   respect unsaved edits and document revisions. Propagate saved changes through
   the existing GitHub sync and provide an updated download.

Composio supports Google Calendar and Gmail OAuth, including custom scopes. It is
an optional integration provider; a direct provider connection fits the app's
existing preference for avoiding a hosted integration relay. Choose the account
provider and connection approach before implementing OAuth and token storage.
No calendar/email account was connected as part of this change.

Primary references:
- https://docs.composio.dev/toolkits/googlecalendar
- https://docs.composio.dev/toolkits/gmail
- https://docs.composio.dev/docs/authentication/controlling-scopes

## Acceptance checks for automatic repair

- A wrong phonetic spelling is repaired when event and relevant email identity agree.
- An explicit user correction overrides a conflicting automated proposal.
- Two similarly named attendees remain unresolved until evidence distinguishes them.
- An unrelated contact/email and an adjacent calendar event cannot rename a participant.
- Quoted instructions in calendar/email/transcript cannot trigger tool calls or egress.
- Original transcript/audio remain unchanged; corrections have provenance and can be undone.
- A calendar attendee is never assigned to a voice solely because they were invited.
