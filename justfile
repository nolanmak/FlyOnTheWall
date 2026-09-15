# FlyOnTheWall build + packaging.
#
# Deliberately hand-rolled rather than cargo-bundle / cargo-packager / cargo-dist:
#   - cargo-dist cannot produce .app bundles at all
#   - cargo-bundle cannot emit a verbatim Info.plist and stamps a UTC timestamp
#     into CFBundleVersion, so builds are not reproducible
#   - cargo-packager works but shells out to the same Apple tools anyway
# Assembly is ~40 lines. See docs/REQUIREMENTS.md section 5.1.
#
# Everything here uses Command Line Tools, not Xcode.app: `xcode-select --install`
# is sufficient for the Apple tools. The other prerequisites (rustup, just,
# cmake, Node for the UI tests) are listed in CONTRIBUTING.md.

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
    node --test crates/fotw-web/tests/ui/*.cjs

lint:
    cargo fmt --all --check
    CARGO_BUILD_JOBS={{jobs}} cargo clippy --workspace --all-targets -- -D warnings

# Dependency licenses, bans and sources, exactly as CI's `deny` job checks them
# (.github/workflows/ci.yml; keep the two commands in step). cargo-deny is
# optional locally, so without it this says so and leaves the gate to CI.
deny:
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v cargo-deny >/dev/null 2>&1; then
        cargo deny check licenses bans sources
    else
        echo "⚠ skipped cargo deny: not installed (cargo install --locked cargo-deny); CI still runs it"
    fi

# Everything CI runs, locally, in CI's order. Where a local run still differs:
#   - CI runs the tests on both ubuntu-latest and macos-15; this runs them here.
#   - CI sets RUSTFLAGS=-D warnings on every job. Locally `lint` runs clippy
#     with -D warnings over every target instead, which fails on the same
#     compiler warnings without giving the other recipes a separate build cache.
#   - `deny` and the Windows half of `seam` print a "skipped" line instead of
#     running when cargo-deny or the x86_64-pc-windows-msvc target is missing.
ci: lint test deny seam
    @echo "✓ ci green"

# The platform seam must not rot into macOS-shaped code. Mirrors CI's `seam`
# job: fotw-audio has to compile for Windows, and no macOS type may appear
# outside fotw-audio/src/platform/macos/.
seam:
    #!/usr/bin/env bash
    set -euo pipefail
    target=x86_64-pc-windows-msvc
    installed=$(rustup target list --installed 2>/dev/null || true)
    if grep -qx "$target" <<<"$installed"; then
        # CI's RUSTFLAGS=-D warnings, applied here too. With --target set, cargo
        # passes RUSTFLAGS only to the Windows artifacts, which no other recipe
        # builds, so the host build cache is untouched.
        RUSTFLAGS="-D warnings" CARGO_BUILD_JOBS={{jobs}} cargo check -p fotw-audio --target "$target"
    else
        echo "⚠ skipped the Windows cross-check: target not installed (rustup target add $target); CI still runs it"
    fi
    if grep -rn 'CMSampleBuffer\|AudioBufferList\|SCStream\|AudioDeviceID\|AudioObjectID' \
         crates/ --include='*.rs' \
         | grep -v 'crates/fotw-audio/src/platform/macos/'; then
        echo "error: macOS types leaked outside fotw-audio/src/platform/macos/" >&2
        exit 1
    fi
    echo "✓ seam intact"

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
    cp packaging/AppIcon.icns "{{app}}/Contents/Resources/AppIcon.icns"
    cp THIRD_PARTY_NOTICES.md "{{app}}/Contents/Resources/THIRD_PARTY_NOTICES.md"
    printf 'APPL????' > "{{app}}/Contents/PkgInfo"
    plutil -lint "{{app}}/Contents/Info.plist"
    echo "✓ assembled {{app}}"

# THIRD_PARTY_NOTICES.md holds the notices and license texts for what fotwd
# links, plus the icon credit, and `bundle` copies it into the app. Run this
# after any Cargo.lock change and commit the result. The cargo-about config
# and template live in packaging/licenses/.
# Regenerate THIRD_PARTY_NOTICES.md from Cargo.lock.
licenses:
    #!/usr/bin/env bash
    set -euo pipefail
    # Pinned so the committed file changes only when the dependencies do.
    # cargo-about 0.9 builds its binary only with the `cli` feature.
    version=0.9.2
    have=$(cargo about --version 2>/dev/null || true)
    if [[ -z "$have" ]]; then
        echo "→ installing cargo-about $version"
        cargo install cargo-about --locked --version "$version" --features cli
    elif [[ "$have" != "cargo-about $version" ]]; then
        echo "error: found $have, but THIRD_PARTY_NOTICES.md is generated with cargo-about $version:" >&2
        echo "    cargo install cargo-about --locked --version $version --features cli" >&2
        exit 1
    fi
    # Offline from here on. Online, cargo-about fetches license files that a
    # crate's package lacks from the crate's git repository, so the output
    # would depend on the network. Offline it uses the standard SPDX text.
    cargo fetch --locked --quiet
    tmp=$(mktemp)
    log=$(mktemp)
    trap 'rm -f "$tmp" "$log"' EXIT
    # A clarification whose checksum no longer matches is only a warning, after
    # which cargo-about falls back to its own scan, so anything on stderr fails,
    # as does a non-zero exit.
    status=0
    cargo about generate --frozen --fail \
        --manifest-path crates/fotwd/Cargo.toml \
        --config packaging/licenses/about.toml \
        --output-file "$tmp" packaging/licenses/about.hbs 2>"$log" || status=$?
    if [[ $status -ne 0 || -s "$log" || ! -s "$tmp" ]]; then
        cat "$log" >&2
        echo "error: cargo-about did not run cleanly (exit status $status)" >&2
        exit 1
    fi
    # about.hbs describes the C libraries these crates compile, as of the crate
    # versions it names. When Cargo.lock moves one, or fotwd stops reaching one,
    # stop so that section is re-checked instead of going stale.
    for krate in libsqlite3-sys openssl-src opusic-sys; do
        # A non-zero exit means the crate is gone from Cargo.lock, or that two
        # versions of it are locked and `-i "$krate"` is ambiguous. cargo's
        # stderr says which.
        if ! resolved=$(cargo tree --frozen -p fotwd -e normal,build \
                --target aarch64-apple-darwin -i "$krate" --depth 0 2>"$log"); then
            cat "$log" >&2
            echo "error: cargo tree could not look up $krate in fotwd's dependency graph; re-check its section in packaging/licenses/about.hbs" >&2
            exit 1
        fi
        # A crate still in Cargo.lock that fotwd no longer reaches, for example
        # one that only another workspace member uses, exits 0 with nothing on
        # stdout. Treat that as missing too.
        if [[ -z "$resolved" ]]; then
            echo "error: $krate is no longer in fotwd's dependency graph; update its section in packaging/licenses/about.hbs" >&2
            exit 1
        fi
        resolved=${resolved%%$'\n'*}
        # -w: the match has to end at a non-word character, so a template that
        # names 0.38.20 does not pass for 0.38.2.
        if ! grep -qwF -- "${resolved/ v/ }" packaging/licenses/about.hbs; then
            echo "error: Cargo.lock resolves $resolved, which packaging/licenses/about.hbs does not name." >&2
            echo "  Check the version and license of the C library it compiles, then update about.hbs." >&2
            exit 1
        fi
    done
    # Not cp: the mktemp file is 0600, and the copy in the app should be
    # readable by every user.
    install -m 0644 "$tmp" THIRD_PARTY_NOTICES.md
    echo "✓ wrote THIRD_PARTY_NOTICES.md"

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
    # The notices the binary's BSD/MIT/Apache dependencies require must ship with it.
    if [[ ! -s "{{app}}/Contents/Resources/THIRD_PARTY_NOTICES.md" ]]; then
        echo "✗ bundle lacks Contents/Resources/THIRD_PARTY_NOTICES.md" >&2; fail=1
    fi
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
#
# What this changes outside the repository, all of it undone by `just
# dev-unsign`: a folder, ~/.fotw-dev-cert unless FOTW_DEV_CERT_DIR says
# otherwise, holding a keychain with the identity in it; that keychain's entry
# in your user keychain search list; and a user-domain trust setting that
# accepts the certificate for code signing. CONTRIBUTING.md has the details.
#
# Works with the openssl macOS ships (/usr/bin/openssl, LibreSSL) and with
# OpenSSL 3; see the pkcs12 step.
dev-sign: (bundle "debug")
    #!/usr/bin/env bash
    set -euo pipefail
    # Owner-only. The folder holds the identity's private key, in a keychain
    # whose password is published below, so these permissions are what keep
    # other accounts on this Mac out of it. chmod as well as umask, so a folder
    # an earlier version of this recipe created world-readable is tightened too.
    (umask 077 && mkdir -p {{quote(dev_cert)}})
    chmod 700 {{quote(dev_cert)}}
    dir=$(cd {{quote(dev_cert)}} && pwd)
    kc="$dir/fotw-dev.keychain-db"
    # The keychain password, `fotw`, is deliberately not a secret. This recipe
    # has to unlock the keychain with no prompt on every run, so whatever the
    # password were, it would be written here in a public repository. It is not
    # what protects the key: the owner-only folder is. The identity is minted
    # on this machine, trusted by this user account only, and never signs a
    # release, which `release-sign` does with a Developer ID identity.

    # Whether the keychain holds a "FlyOnTheWall Dev" certificate together with
    # its private key, trusted or not. Captured before matching; see the
    # pipefail NOTE in verify-bundle.
    has_identity() {
        local out
        out=$(security find-identity -p codesigning "$kc" 2>/dev/null || true)
        [[ "$out" == *"\"{{dev_ident}}\""* ]]
    }

    # A keychain with no identity in it is what a run that stopped partway
    # leaves behind (an earlier version of this recipe created the keychain
    # first, then failed on the openssl macOS ships). Keeping it would make
    # every later run skip setup and fail at codesign with "no identity found",
    # so it is rebuilt.
    if [[ -f "$kc" ]]; then
        if ! security unlock-keychain -p fotw "$kc"; then
            echo "error: $kc exists but does not unlock with this recipe's password." >&2
            echo "  Remove it with \`just dev-unsign\`, then run \`just dev-sign\` again." >&2
            exit 1
        fi
        if ! has_identity; then
            echo "→ $kc has no \"{{dev_ident}}\" identity (an earlier run stopped partway); rebuilding it"
            security delete-keychain "$kc"
        fi
    fi

    if [[ ! -f "$kc" ]]; then
        echo "→ creating persisted dev signing identity in $dir"
        # The key, certificate and .p12 are made before the keychain exists, in
        # a scratch folder removed however this run ends. A failure here leaves
        # nothing behind; a failure after create-keychain leaves a keychain with
        # no identity, which the check above rebuilds on the next run.
        stage=$(mktemp -d "$dir/.new.XXXXXX")
        trap 'rm -rf "$stage"' EXIT
        if ! (umask 077 && openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
            -keyout "$stage/key.pem" -out "$stage/cert.pem" \
            -subj "/CN={{dev_ident}}" \
            -addext "basicConstraints=critical,CA:false" \
            -addext "keyUsage=critical,digitalSignature" \
            -addext "extendedKeyUsage=critical,codeSigning" 2>"$stage/req.log"); then
            cat "$stage/req.log" >&2
            exit 1
        fi
        # -legacy for OpenSSL 3 and later, and only there. OpenSSL 3 defaults to
        # AES-256-CBC + SHA-256 PBKDF2, which `security import` rejects with "MAC
        # verification failed ... (wrong password?)" — an error that sends you
        # hunting a password bug that does not exist. LibreSSL, which is what
        # /usr/bin/openssl is on macOS, has no -legacy and exits on "unknown
        # option"; its default is already the older format (an RC2-40
        # certificate bag, a 3DES key bag and a SHA-1 MAC), and that imports.
        legacy=""
        case "$(openssl version)" in
            "OpenSSL "[3-9]*) legacy="-legacy" ;;
        esac
        # $legacy is unquoted on purpose: empty must mean no argument at all.
        (umask 077 && openssl pkcs12 -export $legacy \
            -inkey "$stage/key.pem" -in "$stage/cert.pem" \
            -out "$stage/dev.p12" -passout pass:fotw)
        security create-keychain -p fotw "$kc"
        security set-keychain-settings -lut 21600 "$kc"
        security unlock-keychain -p fotw "$kc"
        security import "$stage/dev.p12" -k "$kc" -P fotw \
            -T /usr/bin/codesign -T /usr/bin/security
        security set-key-partition-list -S apple-tool:,apple: -s -k fotw "$kc" >/dev/null
        # The private key lives in the keychain from here on. The plaintext
        # key.pem and the .p12 go with the scratch folder rather than sitting
        # beside it as a second copy.
        rm -rf "$stage"
        trap - EXIT
    fi

    # An imported-but-untrusted cert is invisible to codesign, and the error
    # ("no identity found") never mentions trust. Checked on every run, not only
    # after creating the keychain, so a run that stopped before this point is
    # finished by the next one. verify-cert only evaluates; it changes nothing.
    # The certificate is read back out of the keychain, so the one checked is
    # the one codesign will use, and cert.pem stays for dev-unsign.
    pem=$(security find-certificate -c "{{dev_ident}}" -p "$kc")
    printf '%s\n' "$pem" > "$dir/cert.pem"
    if ! security verify-cert -c "$dir/cert.pem" -p codeSign -L -N -q; then
        security add-trusted-cert -r trustRoot -p codeSign -k "$kc" "$dir/cert.pem"
    fi

    security unlock-keychain -p fotw "$kc"
    # codesign resolves identities from the keychain SEARCH LIST, not --keychain.
    # Preserve the existing entries or the login keychain gets unhooked. One
    # entry per array element, so a path with a space in it stays one argument;
    # and the listing is captured first, so a failed `list-keychains` stops the
    # recipe instead of reading as an empty list.
    listing=$(security list-keychains -d user | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//')
    search=()
    listed=0
    while IFS= read -r entry; do
        [[ -n "$entry" ]] || continue
        search+=("$entry")
        if [[ "$entry" == "$kc" || "$entry" -ef "$kc" ]]; then listed=1; fi
    done <<<"$listing"
    if [[ $listed -eq 0 ]]; then
        security list-keychains -d user -s ${search[@]+"${search[@]}"} "$kc"
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
    # The bare CLI binaries and the examples, all under the SAME code
    # identifier as the app -- note the explicit `-i`.
    #
    # macOS keys a keychain item's ACL to the calling code's designated
    # requirement, and the DR pins the code IDENTIFIER, which codesign
    # otherwise derives from the file name. Sign them by default and `fotwd`,
    # `fotw` and each example become three different principals asking for the
    # same `db:masterkey` item, so the second one to run gets an approval
    # dialog -- invisible under launchd, in CI or over SSH, where it blocks
    # forever with no output. They are one product and share one library key,
    # so they get one identifier.
    for bin in target/debug/fotwd target/debug/fotw target/debug/examples/*; do
        # Skip cargo's hashed duplicates, debug info and dep files.
        case "$bin" in *.d|*.dSYM|*-[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]*) continue ;; esac
        if [ -f "$bin" ] && [ -x "$bin" ]; then
            codesign --force --options runtime --timestamp=none \
                -i "{{bundle_id}}" \
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

# Undo what `just dev-sign` changed outside this repository: the code-signing
# trust setting, the keychain search list entry, the dev keychain, and its
# folder (FOTW_DEV_CERT_DIR is honored). Only files dev-sign writes are deleted;
# the folder goes only if that leaves it empty.
#
# Left alone: bundles and binaries already signed with the identity (`just
# clean`), TCC grants macOS recorded for the dev build (the tccutil commands
# dev-sign prints), and your meeting library with its `db:masterkey` item in
# the login keychain. A later `just dev-sign` mints a new identity, and so a new
# designated requirement: macOS asks for audio permission again, and asks you
# to approve the app's access to that keychain item.
dev-unsign:
    #!/usr/bin/env bash
    set -euo pipefail
    dir={{quote(dev_cert)}}
    if [[ -d "$dir" ]]; then dir=$(cd "$dir" && pwd); fi
    kc="$dir/fotw-dev.keychain-db"
    cert="$dir/cert.pem"

    # 1. Trust, first, because it is the one step that can stop: removing the
    # setting needs the certificate, so nothing else is deleted until the
    # setting is gone. The certificate comes out of the keychain when there is
    # one, so the setting removed is the one codesign has been relying on.
    if [[ -f "$kc" ]]; then
        if pem=$(security find-certificate -c "{{dev_ident}}" -p "$kc" 2>/dev/null) && [[ -n "$pem" ]]; then
            printf '%s\n' "$pem" > "$cert"
        fi
    fi
    trust=$(security dump-trust-settings 2>/dev/null || true)
    if grep -qxE "Cert [0-9]+: {{dev_ident}}" <<<"$trust"; then
        if [[ -s "$cert" ]] && security verify-cert -c "$cert" -p codeSign -L -N -q; then
            # remove-trusted-cert raises an authentication dialog on the Mac's
            # own screen (`man security`). Over SSH nobody can answer it and the
            # command waits forever (issue #46), so stop before changing anything.
            if [[ -n "${SSH_CONNECTION:-}" || -n "${SSH_TTY:-}" ]]; then
                echo "error: removing the trust setting needs an authentication dialog on the Mac's screen," >&2
                echo "  which an SSH session cannot answer. Nothing was changed; run this on the Mac itself." >&2
                exit 1
            fi
            echo "→ removing the code-signing trust setting for \"{{dev_ident}}\" (macOS asks you to authenticate)"
            if ! security remove-trusted-cert "$cert"; then
                echo "error: the trust setting was not removed, so nothing else was either." >&2
                echo "  Run \`just dev-unsign\` again and approve the dialog." >&2
                exit 1
            fi
        else
            echo "⚠ a code-signing trust setting named \"{{dev_ident}}\" is for a certificate this recipe"
            echo "  does not have, so it cannot remove it. \`security dump-trust-settings\` lists it."
        fi
    fi

    # 2. The search list: every other entry kept, in its order. Parsed the same
    # way dev-sign parses it.
    listing=$(security list-keychains -d user | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//')
    keep=()
    listed=0
    while IFS= read -r entry; do
        [[ -n "$entry" ]] || continue
        if [[ "$entry" == "$kc" || "$entry" -ef "$kc" ]]; then listed=1; else keep+=("$entry"); fi
    done <<<"$listing"
    if [[ $listed -eq 1 ]]; then
        echo "→ removing $kc from the keychain search list"
        security list-keychains -d user -s ${keep[@]+"${keep[@]}"}
    fi

    # 3. The keychain, and with it the private key.
    if [[ -f "$kc" ]]; then
        echo "→ deleting $kc"
        security delete-keychain "$kc"
    fi

    # 4. The folder. Only names dev-sign creates are removed: key.pem and
    # dev.p12 come from earlier versions of it, .new.* from an interrupted run,
    # and .fl followed by eight hex digits is the empty lock file the Security
    # framework leaves beside a keychain. A FOTW_DEV_CERT_DIR that holds
    # anything else keeps it, and keeps the folder.
    if [[ -d "$dir" ]]; then
        rm -f "$dir/cert.pem" "$dir/key.pem" "$dir/dev.p12"
        rm -f "$dir"/.fl[0-9A-F][0-9A-F][0-9A-F][0-9A-F][0-9A-F][0-9A-F][0-9A-F][0-9A-F]
        rm -rf "$dir"/.new.*
        if rmdir "$dir" 2>/dev/null; then
            echo "→ removed $dir"
        else
            echo "⚠ left $dir in place: it holds files dev-sign did not create"
        fi
    fi
    echo "✓ dev signing identity removed"

# Run the locally-signed app the way a user would. NEVER run the bare binary:
# launching Contents/MacOS/fotwd from a shell makes the TERMINAL the responsible
# process, so the grant attaches to Ghostty/iTerm/Terminal instead of us.
#
# The one exception is the first run, and nothing is captured during it. The
# daemon will not create a meeting library until a person has seen the Recovery
# Key and typed part of it back (crates/fotwd/src/recovery.rs), which needs a
# terminal. A LaunchServices launch has none, so on a first run `serve` refuses,
# and says so only in fotwd.log. So while there is no db.sqlite3, this first
# runs `fotwd list` from inside the bundle, in this terminal. `list` opens the
# library, here creating it behind the ceremony, and prints its meetings; it
# records no audio and asks for no TCC permission, so there is no grant for the
# terminal to lend or to take. It is the same dev-signed binary the app runs,
# so the `db:masterkey` keychain item it creates is one the app can read without
# an approval dialog. A ceremony that is abandoned creates nothing, exits
# non-zero, and stops the recipe before the launch.
run: dev-sign
    #!/usr/bin/env bash
    set -euo pipefail
    # The daemon's default data root: `default_root()`'s parent in
    # crates/fotwd/src/main.rs.
    library="$HOME/Library/Application Support/{{bundle_id}}/db.sqlite3"
    if [[ ! -e "$library" ]]; then
        echo "→ no meeting library yet: creating it in this terminal, where the Recovery Key can be shown"
        "{{app}}/Contents/MacOS/fotwd" list
    fi
    open -a "$(pwd)/{{app}}" --args serve

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
