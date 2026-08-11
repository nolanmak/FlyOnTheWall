# `fotw-shell` — manual QA checklist

Everything in this file is a property **no test in this repository proves**.
GitHub's runners have no window server, no menu bar, no TCC database and no
second application to steal focus from, so the AppKit layer
(`src/platform/macos/`) has no behavioural coverage at all. What automation
does cover is listed at the bottom, so the boundary is explicit rather than
implied.

Run this per release, on each supported macOS (14.4 / 15 / 26). Record
pass/fail per cell. A failure in the **CON-02** section is a release blocker:
those are the cells where the recording indicator is absent or wrong, which is
the requirement `docs/REQUIREMENTS.md` §11.2 makes P0 for legal reasons.

---

## 0. Run the probe first

```sh
cargo run -p fotw-shell --example shell_probe
```

It brings all three surfaces up, asks **AppKit** what it made of them, prints
the answers and exits non-zero if anything is wrong. Every value is read back
off the live objects, so unlike the unit tests it can see a setter that was
silently overwritten by a later call. Expected:

```
activation_policy         1 (1 = Accessory)
status_item_retained      true
hotkeys_registered        2
panel_style_mask          0x8080
panel_is_nonactivating    true   <-- the whole ballgame
panel_can_become_key      true (must be true)
panel_can_become_main     false (must be false)
panel_level               25 (25 = NSStatusWindowLevel)
panel_sharing_type        0 (0 = NSWindowSharingNone)
panel_collection_behavior 0x0151
healthy: true
```

It cannot be a `cargo test`: `libtest` runs every test on a spawned thread and
all three surfaces are main-thread-only. (That guard *is* tested — see
`starting_the_shell_off_the_main_thread_is_refused_not_fatal`.)

This is not a substitute for §1–§3. It proves the properties were configured;
only a real desktop proves they have the effect they are configured for.

| # | Step | Expected | Result |
|---|---|---|---|
| 0.1 | Run the probe on each supported macOS. | `healthy: true`, and every line matches above. | |
| 0.2 | Watch the menu bar while it runs. | The icon appears and disappears. If it never appears, the `NSStatusItem` is being dropped. | |

## 1. The non-activating panel (the one that decides everything)

`NSWindowStyleMask::NonactivatingPanel` is only honoured when passed to
`initWithContentRect:styleMask:backing:defer:`. AppKit calls the private
`-_setPreventsActivation:` during panel *initialization* and never from
`setStyleMask:` (FB16484811). If this regressed, everything below still
*looks* right in a screenshot.

| # | Step | Expected | Result |
|---|---|---|---|
| 1.1 | Join a Zoom call. Start recording. Click the pill's **Stop** button. | Zoom stays the active application throughout — its title bar does not dim, its toolbar does not grey out, and the FlyOnTheWall name never appears in the menu bar. | |
| 1.2 | Type into a Zoom chat box, then click anywhere on the pill's background. | Keystrokes continue to land in Zoom's chat box. The text cursor never leaves it. | |
| 1.3 | With Zoom focused, click the Stop button **once**. | The stop happens on the **first** click. (A borderless window that answers `NO` to `canBecomeKeyWindow` swallows the first click; this is the symptom.) | |
| 1.4 | ⌘-Tab through applications while recording. | FlyOnTheWall does not appear in the switcher. | |

## 2. CON-02 — the indicator is present and legible **(release blockers)**

| # | Step | Expected | Result |
|---|---|---|---|
| 2.1 | Start recording. Full-screen the Zoom window (green button). | The pill **stays visible on top of the full-screen space**. This is the `NSStatusWindowLevel` (25) vs `NSFloatingWindowLevel` (3) trap: at level 3 the pill is fine on a normal desktop and disappears here. The probe (§0) caught this exact regression once already — `setFloatingPanel(true)` assigns the window level and silently overwrote `setLevel(25)`. | |
| 2.2 | While recording, switch to another Space (Ctrl-→). | The pill follows to the new Space (`CanJoinAllSpaces`). | |
| 2.3 | While recording, open Mission Control. | The pill does not fly away with the windows (`Stationary`). | |
| 2.4 | Record with an external monitor attached, then close the lid. | The pill reappears on the remaining screen. Note where it lands. | |
| 2.5 | Record for 65 minutes. | Elapsed reads `1:05:xx`, not `65:xx`, and does not jitter in width. | |
| 2.6 | Speak, then go silent for 30 s. | The level meter moves with speech and empties in silence. | |
| 2.7 | Sleep the Mac mid-recording, wake it. | Elapsed has not gone backwards or reset. | |
| 2.8 | Sanity: read `menu bar` and `pill` simultaneously. | Both show the same elapsed time. | |

## 3. Screen sharing — **do not write marketing copy until this passes**

`setSharingType(NSWindowSharingNone)` is deprecated in favour of
ScreenCaptureKit content filters, so its behaviour must be measured, not
assumed. **No copy anywhere may claim the overlay is invisible during screen
share until 3.1–3.4 all pass**, and even then the claim should be scoped to
the macOS versions actually tested (`docs/REQUIREMENTS.md` §5.5).

| # | Step | Expected | Result |
|---|---|---|---|
| 3.1 | Record, then share your whole screen in Zoom. Have a second participant screenshot it. | Pill absent from what the participant sees. | |
| 3.2 | Same, sharing in Google Meet (Chrome, `getDisplayMedia`). | Pill absent. | |
| 3.3 | Same, using macOS `⇧⌘5` screen recording. | Pill absent. | |
| 3.4 | Same, using a ScreenCaptureKit-based recorder (e.g. CleanShot X, OBS 30+). | Record the result honestly — SCK content filters may ignore `sharingType`. If the pill appears here, say so in `PRIVACY.md`. | |
| 3.5 | The menu-bar item during any of the above. | The menu-bar item **is** visible, as intended. Only the pill is excluded. | |

## 4. Menu-bar item

The `Retained<NSStatusItem>` is held in `Tray::_status_item`. Dropping the last
strong reference removes the icon with no error at all, which reads to a user
as "the app didn't launch".

| # | Step | Expected | Result |
|---|---|---|---|
| 4.1 | Launch. | An icon appears in the menu bar within a second and **stays there** for the whole session. | |
| 4.2 | Start recording. | The icon changes from an outlined ring to a filled red dot, and elapsed time appears beside it. | |
| 4.3 | Switch System Settings → Appearance between Light and Dark while idle. | The idle ring inverts to stay legible (it is a template image). | |
| 4.4 | Same switch **while recording**. | The dot stays **red** in both appearances (it is deliberately not a template). | |
| 4.5 | Click the icon while recording; leave the menu open for 30 s. | The menu does not flicker, close, or reorder; the elapsed row updates in place. | |
| 4.6 | Open the menu during teardown (right after pressing Stop). | "Stop Recording" is greyed out; the status row reads `Saving — …`. | |
| 4.7 | Run with a very crowded menu bar (many items, small display). | The item is not silently dropped; if macOS hides it, note that here. | |

## 5. Global hotkeys

`global-hotkey` uses Carbon `RegisterEventHotKey`, which needs **no**
Accessibility grant. Media keys silently switch it to `CGEventTapCreate`, which
does; `Chord::validate` refuses them, and `to_code` has no arm that can emit
one, so this section is checking the OS side rather than our side.

| # | Step | Expected | Result |
|---|---|---|---|
| 5.1 | Fresh user account, first launch. | **No** Accessibility permission prompt appears, ever. | |
| 5.2 | Confirm System Settings → Privacy & Security → Accessibility. | FlyOnTheWall is **not** listed. | |
| 5.3 | Press ⇧⌘R with Zoom focused. | Recording starts. Focus stays in Zoom. | |
| 5.4 | Press ⇧⌘R again. | Recording stops. | |
| 5.5 | Press and hold ⇧⌘R for two seconds. | Recording toggles **once**, not repeatedly (we act on `Pressed` only, but key repeat is worth confirming). | |
| 5.6 | Launch while another app already owns ⇧⌘R. | Startup fails with a message naming the chord, rather than starting with a dead hotkey. | |
| 5.7 | Press ⇧⌘R during teardown (immediately after Stop). | Nothing happens — no second session starts on top of the flush. | |

## 6. Activation policy

`LSUIElement` in `packaging/Info.plist` **and**
`setActivationPolicy(Accessory)` at startup. Each covers a path the other
misses.

| # | Step | Expected | Result |
|---|---|---|---|
| 6.1 | Launch the signed `.app` from Finder. | No Dock icon appears, not even briefly. No application menu bar. | |
| 6.2 | Launch the bare `fotwd` binary from a terminal (no bundle, so no `Info.plist`). | Still no Dock icon — this is the case `setActivationPolicy` alone covers. | |
| 6.3 | Start via a `launchd` job at login. | Comes up as an accessory; the menu-bar item appears. | |
| 6.4 | With the app running, ⌘-Tab. | FlyOnTheWall is absent from the switcher. | |

## 7. Lifecycle and failure

| # | Step | Expected | Result |
|---|---|---|---|
| 7.1 | Stop a recording. | The pill shows `Saved — mm:ss` for ~4 s, then disappears on its own. | |
| 7.2 | Unplug the audio interface mid-recording. | The pill turns to `Recording failed` with the reason, and **stays up until dismissed** — it must not time out like a successful save. | |
| 7.3 | Quit from the menu while recording. | The session is stopped and flushed before the process exits; no truncated file. | |
| 7.4 | Two-hour soak. | < 3% average CPU, < 50 MB RSS growth (`docs/REQUIREMENTS.md` §5.5). Watch for the 50 ms pump showing up in `powermetrics`. | |
| 7.5 | Two-hour soak, menu never opened. | The `TrayIconEvent` / `MenuEvent` / `GlobalHotKeyEvent` channels do not grow — the pump drains all three unconditionally. Confirm RSS is flat. | |

---

## What automation *does* cover

For contrast, so nobody re-tests these by hand or assumes the reverse:

- **The whole state machine** — `tests/state_machine.rs` (27 tests): phases,
  elapsed formatting, monotone elapsed under a backwards clock, stop/teardown
  ordering, fault acknowledgement, menu enablement, level clamping.
- **CON-02 as a property** — `tests/con02_indicator.rs`: no single input takes
  the indicator down while a session is open; an exhaustive `ShellInput` match
  that fails to compile when a variant is added; a 200 000-input random sweep
  asserting the pill is shown whenever capture is live, that capture commands
  are balanced edges, and that elapsed never runs backwards; a manifest check
  that the crate declares no cargo features; a source scan for suppression-
  shaped identifiers.
- **The media-key ban** — `tests/hotkeys.rs` plus a unit test in
  `src/platform/macos/hotkeys.rs` proving no `MediaKey` can map to a
  `global_hotkey::Code`.
- **The host dispatch path** — `tests/runtime.rs` against a fake host.
- **Menu-bar icon distinctness** — `tests/tray_icon_raster.rs`, on the
  rasteriser rather than on an asset file.
- **Four AppKit constants** — `src/platform/macos/pill.rs` unit tests pin the
  non-activating bit, `NSStatusWindowLevel` (not floating), the four collection
  behaviour bits, and `NSWindowSharingType::None`. These prove the *values*,
  never the behaviour; §1–§3 above are what prove the behaviour.

Mutation testing: 20 seeded defects were introduced one at a time and all 20
were caught by the suite above.
