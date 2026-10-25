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
