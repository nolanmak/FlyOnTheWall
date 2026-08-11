//! macOS system-audio capture via Core Audio process taps.
//!
//! **This is the only directory in the tree permitted to name macOS types.**
//! CI greps for them everywhere else (docs/REQUIREMENTS.md 6.5).
//!
//! # Why taps and not ScreenCaptureKit
//!
//! A tap needs no Apple-granted entitlement, no kernel extension and no
//! virtual audio device, and it does not require the Screen Recording grant.
//! ScreenCaptureKit cannot capture audio without also running the display
//! pipeline, burns a heavier permission, and re-prompts monthly.
//!
//! # What this costs the user, stated honestly
//!
//! The grant is surfaced as "System Audio Recording Only" inside System
//! Settings → Privacy & Security → **Screen & System Audio Recording** — the
//! screen-recording pane, even though we never get screen access. The
//! `tccutil` service is `AudioCapture` (**not** `SystemAudioCaptureRequests`,
//! which several 2026 write-ups cite and which does not exist).
//!
//! # The permission you cannot observe
//!
//! There is no public API to query or pre-request the system-audio grant. The
//! prompt fires only on the first `AudioDeviceStart` of an aggregate device
//! containing a tap, and **a denial delivers silence indistinguishable from a
//! quiet room**. So [`MacOsPlatform::permission`] reports
//! [`PermissionState::Unobservable`] rather than guessing, and the honest test
//! is a round trip: start a tap, play a tone, see whether samples arrive.
//! `fotw doctor` does exactly that.
//!
//! We deliberately do **not** ship AudioCap's private-TCC-framework probe: it
//! is undocumented and its own users report it unreliable.

mod mic;
mod tap;

use std::sync::mpsc::Receiver;

use crate::error::TapError;
use crate::events::{EventBus, PlatformEvent};
use crate::format::FormatRequest;
use crate::ids::{AppInfo, AppRef, DeviceId, DeviceInfo, TapId};
use crate::permission::{Permission, PermissionState, PlatformCaps};
use crate::tap::{AudioPlatform, AudioTap, BoxFuture, SystemScope};

pub use mic::MicTap;
pub use tap::SystemTap;

/// The macOS backend.
#[derive(Debug, Default)]
pub struct MacOsPlatform {
    bus: EventBus,
}

impl MacOsPlatform {
    /// Construct the backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a platform event.
    pub fn emit(&self, event: PlatformEvent) {
        self.bus.emit(event);
    }
}

impl AudioPlatform for MacOsPlatform {
    fn caps(&self) -> PlatformCaps {
        PlatformCaps {
            system_mix: true,
            // Per-app scoping needs CATapDescription.bundleIDs (macOS 26+) or
            // PID translation. Not wired up yet, and advertising a capability
            // we do not have would make the UI offer a control that silently
            // captures everything.
            app_scoped: false,
            exclude_scope: true,
            emits_silence_when_idle: true,
            needs_consent_for_system: true,
        }
    }

    fn permission(&self, permission: Permission) -> PermissionState {
        match permission {
            // See the module docs: there is no API for this, and pretending
            // otherwise would make onboarding lie.
            Permission::SystemAudio => PermissionState::Unobservable,
            Permission::Microphone => PermissionState::NotDetermined,
            Permission::ScreenRecording => PermissionState::NotApplicable,
        }
    }

    fn request_permission(&self, permission: Permission) -> BoxFuture<'static, PermissionState> {
        let state = match permission {
            Permission::SystemAudio => PermissionState::Unobservable,
            Permission::Microphone => PermissionState::NotDetermined,
            Permission::ScreenRecording => PermissionState::NotApplicable,
        };
        Box::pin(async move { state })
    }

    fn mics(&self) -> Vec<DeviceInfo> {
        // Only the default input for now. Enumerating every device is a
        // settings-UI concern and needs no capture code.
        vec![DeviceInfo::new(
            DeviceId::new("default"),
            "Default input",
            true,
        )]
    }

    fn capturable_apps(&self) -> Vec<AppInfo> {
        Vec::new()
    }

    fn open_mic(
        &self,
        _device: &DeviceId,
        _hint: FormatRequest,
    ) -> Result<Box<dyn AudioTap>, TapError> {
        // A separate device and IOProc from the system tap, never a fused
        // aggregate — see the mic module docs.
        Ok(Box::new(MicTap::default_input()?))
    }

    fn open_system(
        &self,
        scope: SystemScope,
        _hint: FormatRequest,
    ) -> Result<Box<dyn AudioTap>, TapError> {
        let excluded: Vec<AppRef> = match scope {
            SystemScope::DefaultOutputMix => Vec::new(),
            SystemScope::AllExcept(apps) => apps,
            SystemScope::Apps(_) => {
                return Err(TapError::unsupported(
                    "per-app capture requires CATapDescription.bundleIDs (macOS 26+); \
                     not implemented yet",
                ));
            }
        };
        Ok(Box::new(SystemTap::new(
            TapId::system_default(),
            &excluded,
        )?))
    }

    fn events(&self) -> Receiver<PlatformEvent> {
        self.bus.subscribe()
    }
}
