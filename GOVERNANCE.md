# Governance

## The commitment

**No feature of FlyOnTheWall will ever be reserved for a paid tier.**

There is no Pro edition, no cloud upsell, no "community edition" with something
deliberately withheld. Every capability the project ships is in this repository
under Apache-2.0.

This is not decoration. It is one of the three things the project is
differentiated on, and the space has a track record that makes the promise worth
writing down:

- `anarlog` gates hosted models behind Pro.
- `meetily` states that speaker diarization is planned for PRO — deliberately
  withheld from the open edition.
- `screenpipe` relicensed away from open source mid-life to a proprietary
  commercial license.

If a future maintainer wants to change this, that is a relicensing decision, and
relicensing requires the consent of contributors — see below.

## Relicensing

The project is Apache-2.0. Changing the license requires the explicit consent of
every contributor holding copyright in code still present in the tree. There is
no CLA and no copyright assignment, precisely so that no single party can
relicense unilaterally.

## Decision making

Technical decisions are made in issues and pull requests, in public. Where a
decision is load-bearing enough that a future contributor would otherwise
re-litigate it, it is written into `docs/REQUIREMENTS.md` with the reasoning and
the rejected alternatives, not just the conclusion.

## Security

Report vulnerabilities privately via GitHub Security Advisories rather than a
public issue.
