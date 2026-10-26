//! Render/event seam for a recorder-driven host as well as the legacy runtime.
use crate::{Chord, MenuAction, Monotonic, PromptChoice, ShellHost, ShellRuntime, ShellView};

/// A user action delivered by the native surfaces. None implies capture success.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceEvent {
    /// A pressed global shortcut.
    Chord(Chord),
    /// A menu selection.
    Menu(MenuAction),
    /// Stop on the visible recording indicator.
    Stop,
    /// OS termination requests cleanup; it never acknowledges a failed save.
    Terminate,
    /// The explicit acknowledgement checkbox changed.
    Acknowledge(bool),
    /// A response to the visible consent prompt.
    Prompt(PromptChoice),
}
/// A host whose frame and commands may follow an asynchronous recorder.
/// Methods run on the native thread and must not perform blocking work.
pub trait SurfaceHost {
    /// A user action; processing must not assume recording has already started.
    fn on_event(&mut self, event: SurfaceEvent, now: Monotonic);
    /// Poll bounded background state and update the frame.
    fn tick(&mut self, now: Monotonic);
    /// The frame to render, using authoritative backend state where available.
    fn view(&self) -> ShellView;
    /// Acknowledgement after the entire frame has been applied to native views.
    fn did_render(&mut self) {}
    /// Whether coordinated shutdown has completed and the event loop may stop.
    fn should_exit(&self) -> bool {
        false
    }
}
impl<H: ShellHost> SurfaceHost for ShellRuntime<H> {
    fn on_event(&mut self, event: SurfaceEvent, now: Monotonic) {
        match event {
            SurfaceEvent::Chord(chord) => {
                self.on_chord(chord, now);
            }
            SurfaceEvent::Menu(action) => self.on_menu(action, now),
            SurfaceEvent::Stop => self.request_stop(),
            SurfaceEvent::Terminate => self.on_menu(MenuAction::Quit, now),
            SurfaceEvent::Acknowledge(acknowledged) => self.acknowledge_prompt(acknowledged),
            SurfaceEvent::Prompt(choice) => self.respond_to_prompt(choice, now),
        }
    }
    fn tick(&mut self, now: Monotonic) {
        Self::tick(self, now);
    }
    fn view(&self) -> ShellView {
        Self::view(self)
    }
}

/// Require a complete visible indicator frame across an AppKit pump cycle.
#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
pub(crate) struct FrameAdmission {
    previous: Option<crate::PillView>,
    previous_visible: bool,
}
#[cfg(any(target_os = "macos", test))]
impl FrameAdmission {
    pub(crate) fn observe(&mut self, frame: Option<&crate::PillView>, visible: bool) -> bool {
        let ready = frame.is_none()
            || (self.previous_visible && visible && self.previous.as_ref() == frame);
        self.previous = frame.cloned();
        self.previous_visible = frame.is_some() && visible;
        ready
    }
}

/// Apply the complete frame before acknowledging it. A failed renderer must not
/// release capture admission. Kept outside AppKit for headless ordering checks.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn render_surface<H: SurfaceHost, E>(
    host: &mut H,
    render: impl FnOnce(&ShellView) -> Result<(), E>,
) -> Result<(), E> {
    render(&host.view())?;
    host.did_render();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeHost, HostCall};
    use std::cell::RefCell;
    use std::rc::Rc;

    struct Host {
        calls: Rc<RefCell<Vec<&'static str>>>,
    }
    impl SurfaceHost for Host {
        fn on_event(&mut self, _: SurfaceEvent, _: Monotonic) {}
        fn tick(&mut self, _: Monotonic) {}
        fn view(&self) -> ShellView {
            self.calls.borrow_mut().push("frame");
            crate::ShellCore::new().view()
        }
        fn did_render(&mut self) {
            self.calls.borrow_mut().push("acknowledge");
        }
    }
    #[test]
    fn render_acknowledgement_follows_complete_successful_render_only() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut host = Host {
            calls: calls.clone(),
        };
        assert_eq!(
            render_surface(&mut host, |_| {
                calls.borrow_mut().push("render");
                Ok::<_, ()>(())
            }),
            Ok(())
        );
        assert_eq!(*calls.borrow(), ["frame", "render", "acknowledge"]);
        calls.borrow_mut().clear();
        assert_eq!(
            render_surface(&mut host, |_| {
                calls.borrow_mut().push("failed render");
                Err("unavailable")
            }),
            Err("unavailable")
        );
        assert_eq!(*calls.borrow(), ["frame", "failed render"]);
    }
    #[test]
    fn capture_admission_requires_an_unchanged_visible_frame_across_a_pump_cycle() {
        let mut core = crate::ShellCore::new();
        core.handle(crate::ShellInput::Start {
            at: Monotonic::ZERO,
            origin: crate::StartOrigin::Menu,
        });
        let frame = core.view().pill.unwrap();
        let mut admission = FrameAdmission::default();
        assert!(!admission.observe(Some(&frame), false));
        assert!(
            !admission.observe(Some(&frame), true),
            "first visible frame has not passed a pump cycle"
        );
        assert!(admission.observe(Some(&frame), true));
        let mut changed = frame.clone();
        changed.detail = "a different attempt/source state".into();
        assert!(!admission.observe(Some(&changed), true));
        assert!(admission.observe(Some(&changed), true));
        assert!(admission.observe(None, false));
        assert!(
            !admission.observe(Some(&changed), true),
            "hidden views must not retain admission"
        );
    }
    #[test]
    fn legacy_runtime_keeps_its_existing_event_routing() {
        let fake = FakeHost::new();
        let mut runtime = ShellRuntime::new(fake.clone());
        SurfaceHost::on_event(
            &mut runtime,
            SurfaceEvent::Menu(MenuAction::OpenNotes),
            Monotonic::ZERO,
        );
        assert_eq!(fake.calls(), [HostCall::OpenNotes]);
        SurfaceHost::on_event(
            &mut runtime,
            SurfaceEvent::Menu(MenuAction::ToggleRecording),
            Monotonic::ZERO,
        );
        assert!(fake.calls().contains(&HostCall::StartCapture));
        SurfaceHost::on_event(&mut runtime, SurfaceEvent::Stop, Monotonic::ZERO);
        assert!(fake.calls().contains(&HostCall::StopCapture));
    }
}
