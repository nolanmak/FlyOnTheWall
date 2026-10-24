#!/usr/bin/env bash
set -euo pipefail
if [[ "$(uname -s)" != Linux ]]; then
    echo 'The keyring interoperability probe requires Linux.' >&2
    exit 1
fi
for tool in dbus-run-session gnome-keyring-daemon cargo; do
    command -v "$tool" >/dev/null || { echo "Missing $tool" >&2; exit 1; }
done
fixture_root=$(mktemp -d)
trap 'rm -rf "$fixture_root"' EXIT
export XDG_DATA_HOME="$fixture_root/data"
export XDG_CONFIG_HOME="$fixture_root/config"
export XDG_RUNTIME_DIR="$fixture_root/runtime"
mkdir -m 700 "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$XDG_RUNTIME_DIR"
export FOTW_ISOLATED_KEYRING_TEST=1
unset GNOME_KEYRING_CONTROL GNOME_KEYRING_PID
# A private bus and temporary keyring, never the user's login bus or keychain.
dbus-run-session -- bash -euo pipefail -c '
    printf "%s" "synthetic-test-password" | gnome-keyring-daemon --unlock --components=secrets
    cargo test --locked --manifest-path tests/keyring-interop/Cargo.toml backends_share -- --nocapture
'
# A fresh process exercises cached initialization failure, not a live keyring.
FOTW_ISOLATED_KEYRING_TEST=unavailable \
DBUS_SESSION_BUS_ADDRESS="unix:path=$fixture_root/no-such-bus" \
    cargo test --locked --manifest-path tests/keyring-interop/Cargo.toml unavailable_service -- --nocapture
