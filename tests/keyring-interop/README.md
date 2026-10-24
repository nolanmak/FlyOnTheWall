# Secret Service backend interoperability

Run `bash scripts/test-keyring-interop.sh` from the repository root on Linux
with `dbus-run-session`, `gnome-keyring-daemon`, `pkg-config` and
`libdbus-1-dev` installed. The CI
`keyring-interop` job does this in a disposable runner. The script creates a
private session bus and temporary XDG directories; it never uses the login
keychain, application service name, or real account names. It fails rather
than pretending to run on another platform.

The probe writes all 256 byte values through the existing zbus backend and
reads them through the proposed DBus backend. It checks account isolation,
updates in both directions, exactly one matching item after updating, creation
by the replacement and deletion in both directions. This proves persistence
compatibility against an actual Secret Service rather than a mocked key store.
The fixture itself does not migrate production code or user credentials.

This is a separate workspace because it intentionally keeps the former backend
for migration checks. Its legacy zbus dependencies have maintenance advisories;
they must not become production dependencies merely to run this test. Its
lockfile is for repeatable CI, not bundled application notices. Production
dependency audits still run independently and must pass before approval.

The probe compiles the production platform adapter directly and uses it for
replacement entries. A separate process checks that an unavailable session
bus refuses initialization and credential creation. Both checks run in CI;
macOS tests do not access the user's Keychain.
