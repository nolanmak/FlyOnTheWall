# Dashboard refresh (#123)

The first pass adapts Muesli's sidebar/history rows, restrained adaptive surfaces,
reading width, and 4-point spacing concepts to the existing FlyOnTheWall web UI.
Reference: https://github.com/Muesli-HQ/muesli (local reference commit
55c207859bcf6e7c33d99568c4f731662d4c08cd; see its design-system/README.md and MuesliTheme.swift).
The CSS and DOM changes are written for this repository; no Muesli source,
icons, logos, statistics, or native components are copied. FlyOnTheWall keeps
its own identity and recording/consent behavior. Green is a local design choice.

The meeting list now exposes the current selection through aria-current,
including search results and list refreshes. Returning to recording clears it.
Search has an explicit accessible name. Light/dark surfaces, reading cards,
focus rings and minimum button heights use local CSS without network assets.
Reduced-motion mode now stops the document spinner animation.

Validation: the new selection test failed with undefined aria-current before
implementation and passes afterward. All 63 Node UI tests pass on the combined
notes/dashboard branch. Rendered evidence and remaining checks are below.

## Source-to-component map

Reference files below are under `native/MuesliNative/Sources/MuesliNativeApp/`
at the pinned revision above. This maps design concepts; CSS values and behavior
are implemented for FlyOnTheWall's existing DOM and API.

| Reference | FlyOnTheWall component | Adaptation |
| --- | --- | --- |
| `MuesliTheme.swift` | `app.css` root tokens, detail headings and reading cards | System fonts, adaptive surfaces, 26px title/15px reading text, 4px spacing increments; local 760px reading width and green accent |
| `SidebarView.swift`, `DashboardRootView.swift` | `main`, `nav#list` and narrow-layout media query | Distinct sidebar surface and constrained history column; stack history above detail on small screens |
| `MeetingListItemView.swift` | `renderList`, `renderHits`, `.meeting` | Title/date/metadata hierarchy and visible selected row; native buttons retain keyboard behavior and expose selection with `aria-current` |
| `MeetingDetailView.swift`, `MeetingNotesView.swift` | `#detail`, `.summary-body`, `.note-body` | Comfortable reading column and separate raised summary/notes surfaces; existing actions and evidence remain intact |

No statistics, folders, provider cards, or native controls are added without
supporting product behavior.

## Rendered comparison

These screenshots use the real embedded UI and API with synthetic meetings in
a temporary encrypted library. The baseline includes the saved-notes editor;
the after images add this dashboard increment. Both use an isolated WebKit view
with a nonpersistent data store. At 390px the detail pane is scrolled to the
notes; wider views show the detail from the top.

| Appearance | Width | Before | After |
| --- | --- | --- | --- |
| Light | 390 | [Before](dashboard-refresh/before-light-390.png) | [After](dashboard-refresh/after-light-390.png) |
| Light | 768 | [Before](dashboard-refresh/before-light-768.png) | [After](dashboard-refresh/after-light-768.png) |
| Light | 1440 | [Before](dashboard-refresh/before-light-1440.png) | [After](dashboard-refresh/after-light-1440.png) |
| Dark | 390 | [Before](dashboard-refresh/before-dark-390.png) | [After](dashboard-refresh/after-dark-390.png) |
| Dark | 768 | [Before](dashboard-refresh/before-dark-768.png) | [After](dashboard-refresh/after-dark-768.png) |
| Dark | 1440 | [Before](dashboard-refresh/before-dark-1440.png) | [After](dashboard-refresh/after-dark-1440.png) |

All six after views preserve current-meeting identity through actual search and
list refresh, expose the search label, keep hidden controls hidden and report
no page horizontal overflow. Measured body, editor, selected-row and selected
metadata text contrast exceeds 4.5:1 in both themes. Note save/navigation and
fresh-view persistence also pass against the same API. No user data, browser
profile, recording device or provider credentials were used.

## Remaining acceptance for #123

The isolated page is not an active browser window: logical focus and an AppKit
Option-Tab event reached a meeting button, but `document.hasFocus()` stayed
false and its focus ring was not drawn. Active-window keyboard/focus-ring and
200% browser zoom checks remain required. Physical recording/Stop, all live
provider/recovery states, IME/mobile selection and long-transcript performance
acceptance also remain open. This first visual increment does not close #123.
The earlier Chrome CDP endpoint returned 404; no Chrome profile or setting was
changed to obtain these WebKit screenshots.


## Incremental live transcript rendering

Partial speech updates reuse the mounted finalized rows and replace only changed
rows. Committed history is re-sorted when finals change, rather than on each
partial. The 200-row display budget and complete finalized-only copy source stay
separate; late earlier finals can still update the displayed speech origin.
Navigation discards the rendering cache with its DOM.

A synthetic 10,000-segment fixture with 40 changing partials reproduced the old
behavior before the change. An isolated, nonpersistent WebKit view ran the real
HTML/CSS/JavaScript against owned loopback static assets, with no app/provider
credentials, capture, clipboard or global input. Each update forced layout.

| Measured work across 40 partial updates | Before | After |
| --- | ---: | ---: |
| Segment rows constructed | 8,000 | 40 |
| History entries passed through ordering | 800,000 | 0 |
| Elapsed script and forced layout in this run | 115 ms | 9 ms |
| Visible segment rows | 200 | 200 |

The fixture retained scrollTop 40, complete copy text (including offscreen first
and last finals), and excluded the partial from copy. The new runtime regression
also checks stable final-node identity and bounded rendering work; all 106 Node
UI tests passed. Timing is one owned synthetic run, not a hardware-wide latency
guarantee. This validates partial updates only: saved full-transcript rendering,
long-history memory, final-arrival trimming while reading, active-window keyboard
and zoom acceptance remain open for #123.


## Saved transcript paging

Saved meetings now render 200 lines at a time with Previous/Next and a labeled
page number. The range states both line numbers and speech-relative times. All
lines remain reachable in order, page changes retain the same controls for
keyboard focus, and each page keeps the full meeting's original speech origin.
Short meetings retain their single transcript surface. Copy/export still uses
the complete source detail, including lines outside the current page; browser
Find searches the mounted page, while the existing meeting search indexes all
saved text. Paging does not reduce the complete source data held for copy.

The runtime regression first failed because opening 4,001 lines constructed all
rows and there was no keyboard-accessible page selector. It now traverses every
page, verifies exact ordered coverage, bounds row construction for each action,
checks full copy, original input identity, invalid input, short/empty meetings,
and stable control identity. All 130 UI tests and full local `just ci` pass.

An owned nonpersistent, nonactivating WKWebView on macOS 26.4 loaded the actual
UI from a temporary encrypted `ui_preview` library. The in-memory fixture
replaced the selected synthetic meeting's segments with 12,001 long Unicode
lines; notes, GitHub and document cards were isolated from asynchronous requests.
The committed [browser fixture](../crates/fotw-web/tests/fixtures/saved-transcript-webkit.js)
measures real element construction and forced layout, reaches the final page,
checks full copy, preserves the 600:00 final offset, and sends a native Tab only
to its own window. Baseline `renderDetail` came from commit
895a5adeab7cd43f6980ef52bb98197e8189c4d7. No capture, models, provider access,
clipboard, global input or user library was involved.

| 12,001-line opening, one 390px light run | Before | After |
| --- | ---: | ---: |
| Created elements, including detail chrome | 48,016 | 819 |
| Attached transcript rows | 12,001 | 200 |
| Script plus forced layout | 659 ms | 41 ms |

These timings describe one fixture run, not a general latency guarantee or a
process-memory measurement. The final six width/appearance checks and actual
200% WebKit page zoom passed; the latter exposed a search-field minimum width
that was corrected to fit its available space. A CSS `zoom` experiment was
replaced with real `WKWebView.pageZoom` so responsive media queries were tested
at the correct effective viewport. The first preview readiness probe timed out
before exercising the renderer; subsequent fresh one-use launches completed.

[All measured results](dashboard-refresh/saved-transcript-results.json),
[390px light](dashboard-refresh/saved-transcript-390-light.png),
[1440px dark](dashboard-refresh/saved-transcript-1440-dark.png), and
[390px at 200% page zoom](dashboard-refresh/saved-transcript-390-pagezoom.png)
record the final candidate. Native Tab selected Next and page changes retained
the focused field. This non-key window does not match `:focus-visible`, so it
cannot prove active-window focus-ring acceptance; no activation was attempted.
The existing focus rule remains. Long-history process memory/GC, active-window
keyboard/screen-reader acceptance, and the full live recording/zoom matrix remain
open for #123. All owned fixture processes and temporary libraries were removed.

### Reading room at 200% zoom

The narrow layout now grows beyond the viewport instead of dividing the space
left beneath wrapped controls into two shrinking panes. The meeting list stays
bounded, and the detail pane receives a viewport-relative height with a minimum
reading height. It retains its own scroll container, which the live transcript
uses for following new speech and preserving history position.

The [WebKit fixture](../crates/fotw-web/tests/fixtures/zoom-reading-webkit.js)
first reproduced a clipped complete utterance at actual 200% page zoom: a
390px-wide window has an effective 195px viewport, where the detail pane was
only 79 CSS pixels high. The 126px utterance could not fit even after scrolling
it into view. The same check passes after the change, including before and after
synthetic Stop. It checks Start, Stop, Return to recording and Copy transcript
visibility/focus, horizontal reflow and saved-meeting selection while another
synthetic meeting records. Copy is focused, never invoked.

Owned nonpersistent, nonactivating WebKit windows loaded the actual UI from
`ui_preview --live-notes` with an explicit debug asset override. Both appearances
passed at physical widths 390, 768 and 1440px, height 1000px, at 100% and actual
200% `WKWebView.pageZoom`. Before screenshots use the CSS from
`21c4cc1c65150839284b0695b6204d97ecc0f0b1`; all other UI assets are identical.
The baseline capture records the clipped geometry without asserting success;
the unchanged assertion fails on that baseline and passes on the candidate.

| 200% zoom | Before | After |
| --- | --- | --- |
| 390px light | [Before](dashboard-refresh/zoom-reading/before-light-390.png) | [After](dashboard-refresh/zoom-reading/after-light-390.png) |
| 768px light | [Before](dashboard-refresh/zoom-reading/before-light-768.png) | [After](dashboard-refresh/zoom-reading/after-light-768.png) |
| 1440px light | [Before](dashboard-refresh/zoom-reading/before-light-1440.png) | [After](dashboard-refresh/zoom-reading/after-light-1440.png) |
| 390px dark | [Before](dashboard-refresh/zoom-reading/before-dark-390.png) | [After](dashboard-refresh/zoom-reading/after-dark-390.png) |
| 768px dark | [Before](dashboard-refresh/zoom-reading/before-dark-768.png) | [After](dashboard-refresh/zoom-reading/after-dark-768.png) |
| 1440px dark | [Before](dashboard-refresh/zoom-reading/before-dark-1440.png) | [After](dashboard-refresh/zoom-reading/after-dark-1440.png) |

[Measured results](dashboard-refresh/zoom-reading/results.json) cover all cases.
The 1440px/200% case retains the desktop layout at effective width 720px.
This is partial #123 evidence, not active-window keyboard, screen-reader,
memory/GC or physical recording acceptance. No capture devices, provider keys,
clipboard, global input or user library were used. All fixture servers were
stopped and their one-use URL files removed.

Review followup: a behavioral RED reproduced losing control focus when Next or
Previous became disabled at a page boundary. The renderer now moves focus to
the page field before disabling the activated button. The native fixture
confirms both boundaries in every final width/appearance/zoom run, and the
runtime copy assertion checks all 4,001 copied lines in order. The browser
fixture is a standard async function; pass its source followed by
`return await runSavedTranscriptFixture();` to `WKWebView.callAsyncJavaScript`.

## Sharing editor lifetime during navigation

Opening saved meetings kept each sharing editor's render callback in a global
cache. That callback rooted a detached panel even after the document load had
finished. A real Node/V8 garbage-collection regression visited 20 read-only
meetings, left their panels, then forced GC: all 20 panels remained reachable
before the fix and none did afterward. A second runtime RED reproduced the
same retention when returning from a saved meeting to the live pane.

Every detail replacement now releases sharing view callbacks. Clean, inactive
editors drop their document/excerpts and are evicted unless small local form
preferences need to survive. Dirty drafts retain their source/revision; busy
operations settle normally. Their completion updates the current mount only,
and inactive clean results are released. A separate forced-GC test collects an
old dirty editor panel, then reopens and saves its original unsaved text. Other
behavior checks cover generation preferences, one requested download completing
after navigation, and an offscreen failed save preserving text for retry.

The [owned WebKit fixture](../crates/fotw-web/tests/fixtures/sharing-lifecycle-webkit.js)
uses the real `ui_preview` library/server and embedded assets. Sharing responses
are synthetic; download intents are captured as strings. At 1440px/light and
390px/dark it edits Unicode text, switches meeting/live views, saves using the
original revision, navigates 20 times, and reopens the saved text. Both cases
pass with exactly one download intent and zero actual downloads. Served script
hashes match the source files. On macOS 26.4, the example runs from its owned
build output and the runner uses a nonactivating, nonpersistent WKWebView.
Append `return await runSharingLifecycleFixture();` to the fixture source passed
to `WKWebView.callAsyncJavaScript`. The screenshots were visually inspected:

- [1440px light](dashboard-refresh/sharing-lifecycle-light-1440.png),
  [measurements](dashboard-refresh/sharing-lifecycle-light-1440.json).
- [390px dark](dashboard-refresh/sharing-lifecycle-dark-390.png),
  [measurements](dashboard-refresh/sharing-lifecycle-dark-390.json).

All 139 UI tests and the full local repository checks pass. Node's forced-GC
evidence proves the detached-panel regression in that engine; the native WebKit
run verifies functional preservation, not browser heap reclamation or whole
process memory. Dirty drafts and pending operations intentionally remain live.
This advances #123 without closing its full memory/accessibility/physical
acceptance. No capture, provider, credential, clipboard, installed app or user
library was used. Owned preview servers, temporary libraries and one-use URL
files were removed; the fixture stores no real conversation data.
