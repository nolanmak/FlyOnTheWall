//! LaunchServices reopen and OS Quit are requests, never capture commands.
use crate::bootstrap::AppRequests;
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSApplicationDelegate, NSApplicationTerminateReply};
use objc2_foundation::{NSObject, NSObjectProtocol};

#[derive(Default)]
pub(crate) struct DelegateIvars {
    requests: AppRequests,
}
define_class!(
    // SAFETY: NSObject adds no subclassing requirements; there is no custom Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FotwNativeAppDelegate"]
    #[ivars = DelegateIvars]
    pub(crate) struct AppDelegate;
    // SAFETY: NSObjectProtocol adds no safety requirements.
    unsafe impl NSObjectProtocol for AppDelegate {}
    // SAFETY: all callbacks execute on AppKit main and only set bounded flags.
    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(&self, _sender: &NSApplication, _has_windows: bool) -> bool {
            self.ivars().requests.request_open();
            // The native host opens a fresh dashboard handoff; do not activate
            // or unhide a recording panel as AppKit's default reopen behavior.
            false
        }
        #[unsafe(method(applicationShouldTerminate:))]
        fn terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            self.ivars().requests.request_quit();
            // Preserve Rust teardown and the capture/persistence gate rather
            // than letting AppKit exit immediately from inside this callback.
            NSApplicationTerminateReply::TerminateCancel
        }
    }
);
impl AppDelegate {
    pub(crate) fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars::default());
        // SAFETY: the superclass initializer has the standard NSObject signature.
        unsafe { msg_send![super(this), init] }
    }
    pub(crate) fn take_open(&self) -> bool {
        self.ivars().requests.take_open()
    }
    pub(crate) fn take_quit(&self) -> bool {
        self.ivars().requests.take_quit()
    }
}
