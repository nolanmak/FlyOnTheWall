# System audio exclusion backend

The native backend now translates Unix PIDs to Core Audio process object IDs
before building a global-except tap. It preserves exact bundle identifiers on
macOS 26 and enables process restoration. Unsupported selectors, unresolved
process IDs and invalid bundle identifiers fail rather than silently selecting
the whole mix. Below macOS 26, bundle-based exclusions are explicitly refused;
PID exclusions remain supported after native translation.

This is the backend foundation for #215. Acceptance in #215 remains open.
No Spotify/Brave settings are exposed by this change, and the recorder still
uses its existing default scope until that follow-up connects the policy.
Complete helper coverage, late starts/restarts and actual waveform exclusion
need physical capture qualification. A descriptor property check cannot prove
OS audio routing.

Regression-first tests reproduced five failures in the original behavior;
all six planner tests now pass. Full audio tests and strict all-target audio
Clippy pass on macOS 26.4. An owned native descriptor test verifies that exact
bundle IDs and process restoration reach AppKit/Core Audio's description object
without creating a tap, opening a device or invoking a capture permission.

Native contracts: Apple documents `processes` as AudioObjectIDs (not PIDs),
`bundleIDs` as exact process bundle IDs and `processRestoreEnabled` as retaining
process identities across exit/restart. The latter two are macOS 26 APIs.
See [CATapDescription](https://developer.apple.com/documentation/coreaudio/catapdescription).

Murmur captures microphone audio only. Excluding system applications cannot
remove music that physically reaches the microphone; headphones avoid that
path. This backend change does not change Murmur capture.
