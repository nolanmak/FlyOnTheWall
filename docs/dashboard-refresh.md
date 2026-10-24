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
implementation and passes afterward; all 48 Node UI tests pass. This proves
selection behavior, not visual accessibility. Browser checks at 390/768/1440px,
both themes, zoom, keyboard navigation, contrast, and all loading/error states
are still required before #123 is complete.

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
supporting product behavior. Screenshots and rendered contrast/keyboard/zoom
checks remain outstanding; 48 passing Node tests do not establish those results.
The local Chrome listener at port 9222 returned HTTP 404 for `/json/version`
during validation, so no browser/profile was launched or replaced to obtain them.
