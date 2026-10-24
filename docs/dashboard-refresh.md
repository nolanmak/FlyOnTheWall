# Dashboard refresh (#123)

The first pass adapts Muesli's sidebar/history rows, restrained adaptive surfaces,
reading width, and 4-point spacing concepts to the existing FlyOnTheWall web UI.
Reference: https://github.com/Muesli-HQ/muesli (local reference commit
55c2078; see its design-system/README.md and MuesliTheme.swift).
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
