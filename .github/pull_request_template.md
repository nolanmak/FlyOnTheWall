## Summary

<!-- What changes and why. Link the issue if there is one, e.g. "Closes #123". -->

## Testing

- [ ] `just ci` passes locally.
- [ ] Tested manually on macOS. Required when the change touches audio capture (`crates/fotw-audio/src/platform/macos`), the AppKit shell, signing or packaging, or the dashboard UI: CI has no audio device, no TCC grant and no browser, so it cannot show these work. Say below what you ran, how the app was started, and on which macOS version.

<!-- e.g. "just run on macOS 15.6: recorded a two-minute call on AirPods, stopped it from the dashboard, the transcript landed." -->

## Data and provenance

- [ ] This PR adds no real meeting data or secrets: no recordings, transcripts, meeting titles, attendee or participant names, calendar details, API keys or Recovery Keys, whether in code, tests, fixtures, screenshots or logs. All test data is synthetic.
- [ ] I wrote this code, or I have stated its origin and license in this description (see [CONTRIBUTING.md](https://github.com/nolanmak/FlyOnTheWall/blob/main/CONTRIBUTING.md)).
