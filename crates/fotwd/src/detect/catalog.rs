//! Which applications are conferencing applications.
//!
//! A table rather than a heuristic. Matching on window titles or process names
//! ("does the name contain 'meet'") is how a detector ends up firing on a
//! Finder window called *Meeting notes*, and every false positive here is a
//! consent problem rather than an annoyance — see the module docs of
//! [`crate::detect`].
//!
//! # Bundle ids, and which of these are actually verified
//!
//! Verified by observation on macOS 26.3 (they appeared in Core Audio's own
//! process list on this machine): `us.zoom.xos`, `com.tinyspeck.slackmacgap`
//! (+ `.helper`), `com.google.Chrome` (+ `.helper`), `com.apple.Safari`,
//! `net.whatsapp.WhatsApp`, `com.apple.FaceTime`.
//!
//! **Not verified, taken from vendor documentation and bug trackers:**
//! `com.microsoft.teams2`, `com.microsoft.teams`, `Cisco-Systems.Spark`,
//! `com.webex.meetingmanager`, `com.hnc.Discord`, `com.skype.skype`,
//! `com.microsoft.edgemac`, `org.mozilla.firefox`, `com.brave.Browser`,
//! `company.thebrowser.Browser`. A wrong id here fails *closed* — the app is
//! simply never detected, and the user presses record themselves — which is
//! the right direction for a mistake in this table to point.

/// One application the detector knows how to recognise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConferencingApp {
    /// Stable key, used for "never detect from this app" and in the audit log.
    /// Always the main bundle id, never a helper's.
    pub key: &'static str,
    /// What the prompt calls it.
    pub name: &'static str,
    /// Bundle ids that mean "this app". A client matches an entry exactly, or
    /// as a `<entry>.` prefix, which is how helper processes
    /// (`com.google.Chrome.helper`) resolve back to their parent.
    pub bundles: &'static [&'static str],
    /// Whether this is a general-purpose browser.
    ///
    /// Browsers need a **stronger** signal than dedicated conferencing apps:
    /// a browser holding the microphone might be Google Meet, or it might be
    /// a voice message, a dictation box or a speech-to-text demo. Requiring
    /// both directions of audio is what separates a call from a recording.
    pub browser: bool,
}

impl ConferencingApp {
    /// Whether a client's bundle id belongs to this app.
    #[must_use]
    pub fn matches(&self, bundle_id: &str) -> bool {
        self.bundles.iter().any(|b| {
            bundle_id == *b
                || (bundle_id.len() > b.len()
                    && bundle_id.starts_with(b)
                    && bundle_id.as_bytes()[b.len()] == b'.')
        })
    }
}

/// The shipped table.
///
/// Ordered most-specific-first only for determinism; the matcher requires an
/// exact or dotted-prefix hit, so ordering cannot change a decision.
pub const APPS: &[ConferencingApp] = &[
    ConferencingApp {
        key: "us.zoom.xos",
        name: "Zoom",
        bundles: &["us.zoom.xos"],
        browser: false,
    },
    ConferencingApp {
        key: "com.microsoft.teams2",
        name: "Microsoft Teams",
        bundles: &["com.microsoft.teams2", "com.microsoft.teams"],
        browser: false,
    },
    ConferencingApp {
        key: "com.tinyspeck.slackmacgap",
        name: "Slack",
        bundles: &["com.tinyspeck.slackmacgap"],
        browser: false,
    },
    ConferencingApp {
        key: "Cisco-Systems.Spark",
        name: "Webex",
        bundles: &["Cisco-Systems.Spark", "com.webex.meetingmanager"],
        browser: false,
    },
    ConferencingApp {
        key: "com.hnc.Discord",
        name: "Discord",
        bundles: &["com.hnc.Discord"],
        browser: false,
    },
    ConferencingApp {
        key: "com.skype.skype",
        name: "Skype",
        bundles: &["com.skype.skype"],
        browser: false,
    },
    ConferencingApp {
        key: "com.apple.FaceTime",
        name: "FaceTime",
        bundles: &["com.apple.FaceTime"],
        browser: false,
    },
    ConferencingApp {
        key: "net.whatsapp.WhatsApp",
        name: "WhatsApp",
        bundles: &["net.whatsapp.WhatsApp"],
        browser: false,
    },
    ConferencingApp {
        key: "com.google.Chrome",
        name: "Chrome",
        bundles: &["com.google.Chrome"],
        browser: true,
    },
    ConferencingApp {
        key: "com.apple.Safari",
        name: "Safari",
        bundles: &["com.apple.Safari", "com.apple.WebKit"],
        browser: true,
    },
    ConferencingApp {
        key: "com.microsoft.edgemac",
        name: "Edge",
        bundles: &["com.microsoft.edgemac"],
        browser: true,
    },
    ConferencingApp {
        key: "org.mozilla.firefox",
        name: "Firefox",
        bundles: &["org.mozilla.firefox"],
        browser: true,
    },
    ConferencingApp {
        key: "com.brave.Browser",
        name: "Brave",
        bundles: &["com.brave.Browser"],
        browser: true,
    },
    ConferencingApp {
        key: "company.thebrowser.Browser",
        name: "Arc",
        bundles: &["company.thebrowser.Browser"],
        browser: true,
    },
];

/// Find the app a bundle id belongs to.
#[must_use]
pub fn lookup(bundle_id: &str) -> Option<&'static ConferencingApp> {
    APPS.iter().find(|a| a.matches(bundle_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_resolve_to_their_parent_app() {
        assert_eq!(
            lookup("com.google.Chrome.helper").map(|a| a.key),
            Some("com.google.Chrome")
        );
        assert_eq!(
            lookup("com.google.Chrome.helper.Renderer").map(|a| a.key),
            Some("com.google.Chrome")
        );
        assert_eq!(
            lookup("com.tinyspeck.slackmacgap.helper").map(|a| a.key),
            Some("com.tinyspeck.slackmacgap")
        );
    }

    #[test]
    fn a_prefix_that_is_not_a_bundle_boundary_does_not_match() {
        // The failure this prevents: `com.google.ChromeRemoteDesktop` being
        // read as Chrome, and a remote-desktop session prompting to record.
        assert!(lookup("com.google.ChromeRemoteDesktop").is_none());
        assert!(lookup("us.zoom.xosuite").is_none());
    }

    #[test]
    fn ordinary_applications_are_not_conferencing_applications() {
        for id in [
            "com.spotify.client",
            "com.apple.Music",
            "com.apple.CoreSpeech",
            "com.apple.podcasts",
            "com.flyonthewall.fotw",
            "",
        ] {
            assert!(lookup(id).is_none(), "{id} was matched as a meeting app");
        }
    }

    #[test]
    fn every_key_is_one_of_its_own_bundle_ids() {
        // The key is what "never for this app" is stored under and what the
        // audit log records. A key that no client can ever carry would make
        // suppression silently ineffective.
        for app in APPS {
            assert!(
                app.bundles.contains(&app.key),
                "{} is not among its own bundle ids",
                app.key
            );
            assert!(app.matches(app.key));
        }
    }
}
