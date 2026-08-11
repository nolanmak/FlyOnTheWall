# FlyOnTheWall build + packaging.
#
# Deliberately hand-rolled rather than cargo-bundle / cargo-packager / cargo-dist:
#   - cargo-dist cannot produce .app bundles at all
#   - cargo-bundle cannot emit a verbatim Info.plist and stamps a UTC timestamp
#     into CFBundleVersion, so builds are not reproducible
#   - cargo-packager works but shells out to the same Apple tools anyway
# Assembly is ~40 lines. See docs/REQUIREMENTS.md section 5.1.
#
# Everything here uses Command Line Tools, not Xcode.app. `xcode-select --install`
# is sufficient.

set shell := ["bash", "-euo", "pipefail", "-c"]

bundle_id  := "com.flyonthewall.fotw"
app_name   := "FlyOnTheWall"
build_dir  := "packaging/build"
app        := build_dir / app_name + ".app"
dev_cert   := env_var_or_default("FOTW_DEV_CERT_DIR", env_var("HOME") / ".fotw-dev-cert")
dev_ident  := "FlyOnTheWall Dev"
# Keep local builds off every core so an interactive machine stays usable.
jobs       := env_var_or_default("CARGO_BUILD_JOBS", "3")

default:
    @just --list

# ---------------------------------------------------------------- build/test

build:
    CARGO_BUILD_JOBS={{jobs}} cargo build --workspace

test:
    CARGO_BUILD_JOBS={{jobs}} cargo test --workspace

lint:
    cargo fmt --all --check
    CARGO_BUILD_JOBS={{jobs}} cargo clippy --workspace --all-targets -- -D warnings

# Everything CI runs, locally, in CI's order.
ci: lint test seam
    @echo "✓ ci green"

# The platform seam must not rot into macOS-shaped code.
seam:
    @if grep -rn 'CMSampleBuffer\|AudioBufferList\|SCStream\|AudioDeviceID\|AudioObjectID' \
         crates/ --include='*.rs' \
         | grep -v 'crates/fotw-audio/src/platform/macos/'; then \
        echo "error: macOS types leaked outside fotw-audio/src/platform/macos/" >&2; \
        exit 1; \
    fi
    @echo "✓ seam intact"

# ---------------------------------------------------------------- bundle

# Assemble FlyOnTheWall.app around the daemon binary.
bundle profile="release":
    #!/usr/bin/env bash
    set -euo pipefail
    CARGO_BUILD_JOBS={{jobs}} cargo build -p fotwd {{ if profile == "release" { "--release" } else { "" } }}
    rm -rf "{{app}}"
    mkdir -p "{{app}}/Contents/MacOS" "{{app}}/Contents/Resources"
    cp "target/{{profile}}/fotwd" "{{app}}/Contents/MacOS/fotwd"
    cp packaging/Info.plist "{{app}}/Contents/Info.plist"
    printf 'APPL????' > "{{app}}/Contents/PkgInfo"
    plutil -lint "{{app}}/Contents/Info.plist"
    echo "✓ assembled {{app}}"

# Assert the bundle carries everything TCC needs. Run in CI and before release:
# each of these failing produces silent capture rather than an error.
verify-bundle:
    #!/usr/bin/env bash
    set -euo pipefail
    fail=0
    plist="{{app}}/Contents/Info.plist"
    for key in NSAudioCaptureUsageDescription NSMicrophoneUsageDescription \
               NSCalendarsFullAccessUsageDescription LSUIElement \
               LSMinimumSystemVersion CFBundleIdentifier; do
        if ! /usr/libexec/PlistBuddy -c "Print :$key" "$plist" >/dev/null 2>&1; then
            echo "✗ Info.plist missing $key" >&2; fail=1
        fi
    done
    # A usage description that exists but is empty still suppresses the prompt.
    for key in NSAudioCaptureUsageDescription NSMicrophoneUsageDescription; do
        v=$(/usr/libexec/PlistBuddy -c "Print :$key" "$plist" 2>/dev/null || true)
        if [[ -z "${v// }" ]]; then echo "✗ $key is empty" >&2; fail=1; fi
    done
    # Every embedded Mach-O must carry the entitlement, not just the outer app.
    # The classic failure is the entitlement on the app but missing on a helper,
    # which suppresses the TCC prompt entirely.
    # NOTE: capture into a variable before matching. `codesign ... | grep -q`
    # under `set -o pipefail` is a false-negative generator: grep -q exits on
    # the first match, codesign dies of SIGPIPE (141), pipefail propagates that
    # as a failed pipeline, and `!` inverts it into a bogus error.
    while IFS= read -r bin; do
        ents=$(codesign -d --entitlements :- "$bin" 2>/dev/null || true)
        case "$ents" in
            *com.apple.security.device.audio-input*) ;;
            *) echo "✗ $bin lacks com.apple.security.device.audio-input" >&2; fail=1 ;;
        esac
        info=$(codesign -d --verbose=2 "$bin" 2>&1 || true)
        case "$info" in
            *"(runtime)"*) ;;
            *) echo "✗ $bin is not hardened-runtime signed" >&2; fail=1 ;;
        esac
    done < <(find "{{app}}/Contents/MacOS" -type f -perm +111)
    [[ $fail -eq 0 ]] && echo "✓ bundle verified" || exit 1

# ---------------------------------------------------------------- dev signing

# Create (once) a PERSISTED self-signed identity and sign the local bundle with it.
#
# This is the only supported way to build and run locally. An ad-hoc signature
# (`codesign -s -`) mints a cdhash-based designated requirement that changes on
# every rebuild, so macOS treats each build as a brand-new app and drops the TCC
# grant. Worse: an unsigned binary run from a terminal can INHERIT the terminal's
# grant and capture real audio with no prompt — your machine reports success
# while users get silence.
dev-sign: (bundle "debug")
    #!/usr/bin/env bash
    set -euo pipefail
    kc="{{dev_cert}}/fotw-dev.keychain-db"
    mkdir -p "{{dev_cert}}"
    if [[ ! -f "$kc" ]]; then
        echo "→ creating persisted dev signing identity in {{dev_cert}}"
        security create-keychain -p fotw "$kc"
        security set-keychain-settings -lut 21600 "$kc"
        security unlock-keychain -p fotw "$kc"
        openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
            -keyout "{{dev_cert}}/key.pem" -out "{{dev_cert}}/cert.pem" \
            -subj "/CN={{dev_ident}}" \
            -addext "basicConstraints=critical,CA:false" \
            -addext "keyUsage=critical,digitalSignature" \
            -addext "extendedKeyUsage=critical,codeSigning" 2>/dev/null
        # -legacy is REQUIRED: OpenSSL 3 defaults to AES-256-CBC + SHA-256 PBKDF2,
        # which `security import` rejects with "MAC verification failed ... (wrong
        # password?)" — an error that sends you hunting a password bug that does
        # not exist.
        openssl pkcs12 -export -legacy \
            -inkey "{{dev_cert}}/key.pem" -in "{{dev_cert}}/cert.pem" \
            -out "{{dev_cert}}/dev.p12" -passout pass:fotw
        security import "{{dev_cert}}/dev.p12" -k "$kc" -P fotw \
            -T /usr/bin/codesign -T /usr/bin/security
        security set-key-partition-list -S apple-tool:,apple: -s -k fotw "$kc" >/dev/null
        # An imported-but-untrusted cert is invisible to codesign, and the error
        # ("no identity found") never mentions trust.
        security add-trusted-cert -r trustRoot -p codeSign -k "$kc" "{{dev_cert}}/cert.pem"
    fi
    security unlock-keychain -p fotw "$kc"
    # codesign resolves identities from the keychain SEARCH LIST, not --keychain.
    # Preserve the existing entries or the login keychain gets unhooked.
    current=$(security list-keychains -d user | sed 's/[[:space:]]*"//g;s/"//g')
    if ! grep -qF "$kc" <<< "$current"; then
        security list-keychains -d user -s $current "$kc"
    fi
    # --timestamp=none in dev only: Apple's timestamp server is a needless
    # network dependency and point of failure for contributors.
    codesign --force --options runtime --timestamp=none \
        --entitlements packaging/entitlements.plist \
        --sign "{{dev_ident}}" "{{app}}"
    # Also sign the bare CLI binaries. macOS keys a keychain item's ACL to the
    # calling code's signature, so an ad-hoc-signed CLI presents a NEW identity
    # on every rebuild and the system raises an approval dialog each time --
    # which, run from a script, from launchd or over SSH, blocks forever with
    # no output at all.
    for bin in target/debug/fotwd target/debug/fotw; do
        if [ -f "$bin" ]; then
            codesign --force --options runtime --timestamp=none \
                --entitlements packaging/entitlements.plist \
                --sign "{{dev_ident}}" "$bin"
        fi
    done
    echo ""
    echo "Designated Requirement (stable across rebuilds if this looks like an identifier, not a cdhash):"
    codesign -d -r- "{{app}}" 2>&1 | sed 's/^/    /'
    echo ""
    echo "If capture misbehaves, reset the grant with:"
    echo "    tccutil reset AudioCapture {{bundle_id}}"
    echo "    tccutil reset Microphone   {{bundle_id}}"
    echo "(the service is AudioCapture — 'SystemAudioCaptureRequests', which several"
    echo " 2026 blog posts cite, does not exist)"

# Run the locally-signed app the way a user would. NEVER run the bare binary:
# launching Contents/MacOS/fotwd from a shell makes the TERMINAL the responsible
# process, so the grant attaches to Ghostty/iTerm/Terminal instead of us.
run: dev-sign
    open -a "$(pwd)/{{app}}"

# ---------------------------------------------------------------- release

# Order matters and deviating breaks things silently:
#   sign nested -> sign bundle -> ditto zip -> notarize -> staple -> re-ditto
# `codesign --force` after stapling wipes Contents/CodeResources, and zipping
# before stapling ships an app that fails Gatekeeper on a machine that is
# offline at first launch.
release-sign identity:
    #!/usr/bin/env bash
    set -euo pipefail
    find "{{app}}/Contents/MacOS" -type f -perm +111 -mindepth 2 -print0 \
      | xargs -0 -r -n1 codesign --force --options runtime --timestamp \
            --entitlements packaging/entitlements.plist --sign "{{identity}}"
    # --timestamp is not optional: omitting it passes local verification and is
    # rejected only after the round trip to Apple.
    codesign --force --options runtime --timestamp \
        --entitlements packaging/entitlements.plist \
        --sign "{{identity}}" "{{app}}"
    codesign --verify --deep --strict --verbose=2 "{{app}}"
    just verify-bundle

notarize keychain_profile:
    #!/usr/bin/env bash
    set -euo pipefail
    zip="{{build_dir}}/{{app_name}}.zip"
    /usr/bin/ditto -c -k --keepParent "{{app}}" "$zip"
    xcrun notarytool submit "$zip" --keychain-profile "{{keychain_profile}}" --wait
    xcrun stapler staple "{{app}}"
    xcrun stapler validate "{{app}}"
    rm -f "$zip"
    /usr/bin/ditto -c -k --keepParent "{{app}}" "$zip"
    spctl -a -vvv -t exec "{{app}}"
    echo "✓ notarized and stapled"

clean:
    rm -rf "{{build_dir}}" target
