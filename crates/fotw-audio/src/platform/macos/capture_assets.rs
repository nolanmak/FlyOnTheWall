//! Native operations for the tested registration lifecycle. No capture is
//! started by construction; the owner calls register/start explicitly.
use super::capture_lifecycle::{CaptureAssets, Failure};
use super::input_format::InputFormatWatch;
use crate::TapError;
use std::sync::Mutex;
use std::sync::{Arc, atomic::AtomicBool};

// Retained native registrations survive a tap object and a recording session.
// Fence the whole process, not just a supervisor instance, until restart.
static MIC_FAILURE: Mutex<Option<Failure>> = Mutex::new(None);
static SYSTEM_FAILURE: Mutex<Option<Failure>> = Mutex::new(None);
use cidre::{
    arc,
    core_audio::{self as ca, AggregateDevice, Device, DeviceIoBlock, DeviceIoProcId, TapGuard},
    dispatch,
};
use std::mem::ManuallyDrop;

enum DeviceOwner {
    Physical(Device),
    Aggregate(ManuallyDrop<AggregateDevice>),
}
pub(super) struct NativeAssets {
    device: Option<DeviceOwner>,
    tap: Option<ManuallyDrop<TapGuard>>,
    block: Option<arc::R<DeviceIoBlock>>,
    queue: Option<arc::R<dispatch::Queue>>,
    input_format: Option<InputFormatWatch>,
}
// Native handles are controlled by one owner; callbacks have their own serial
// dispatch queue and retain only the existing synchronized capture state.
unsafe impl Send for NativeAssets {}
impl NativeAssets {
    pub fn check_mic() -> Result<(), TapError> {
        Self::check(&MIC_FAILURE)
    }
    pub fn check_system() -> Result<(), TapError> {
        Self::check(&SYSTEM_FAILURE)
    }
    fn check(failure: &Mutex<Option<Failure>>) -> Result<(), TapError> {
        failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(Ok(()), |f| Err(f.error()))
    }
    pub fn mic(device: Device) -> Self {
        Self {
            device: Some(DeviceOwner::Physical(device)),
            tap: None,
            block: None,
            queue: None,
            input_format: None,
        }
    }
    pub fn watch_mic_format(&mut self, changed: Arc<AtomicBool>) -> Result<(), TapError> {
        let device = self.device();
        let watcher = self.input_format.insert(InputFormatWatch::new(changed));
        // Own each successful registration before reading a changing topology.
        for target in InputFormatWatch::device_targets(device) {
            watcher.add(target).map_err(TapError::platform)?;
        }
        let address =
            ca::PropSelector::DEVICE_STREAMS.addr(ca::PropScope::INPUT, ca::PropElement::MAIN);
        let streams: Vec<ca::Stream> = device
            .prop_vec(&address)
            .map_err(|e| TapError::platform(format!("Cannot monitor microphone streams: {e:?}")))?;
        if streams.is_empty() {
            return Err(TapError::FormatUnsupported(
                "Microphone has no input streams".into(),
            ));
        }
        for stream in streams {
            watcher
                .add((
                    stream.0,
                    ca::PropSelector::STREAM_VIRTUAL_FORMAT.global_addr(),
                ))
                .map_err(TapError::platform)?;
        }
        Ok(())
    }
    /// Own the process tap before any fallible property/aggregate setup.
    pub fn system(tap: TapGuard) -> Self {
        Self {
            device: None,
            tap: Some(ManuallyDrop::new(tap)),
            block: None,
            queue: None,
            input_format: None,
        }
    }
    pub fn system_tap(&self) -> &TapGuard {
        self.tap.as_ref().expect("owned process tap")
    }
    pub fn set_system_device(&mut self, device: AggregateDevice) {
        self.device = Some(DeviceOwner::Aggregate(ManuallyDrop::new(device)));
    }
    pub fn set_callback(&mut self, block: arc::R<DeviceIoBlock>, queue: arc::R<dispatch::Queue>) {
        self.block = Some(block);
        self.queue = Some(queue);
    }
    fn device(&self) -> Device {
        let device = match self.device.as_ref().expect("configured capture device") {
            DeviceOwner::Physical(device) => device,
            DeviceOwner::Aggregate(device) => device.as_ref(),
        };
        Device(device.0)
    }
}
fn check(status: i32, operation: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{operation} failed: {status}"))
    }
}
impl CaptureAssets for NativeAssets {
    type Proc = DeviceIoProcId;
    fn register(&mut self) -> Result<Self::Proc, String> {
        // Only a successfully prepared owner may register its callback.
        let device = self.device();
        let queue = self.queue.as_ref().expect("configured callback queue");
        let block = self.block.as_mut().expect("configured callback block");
        device
            .create_io_proc_id_with_block(Some(queue), block)
            .map_err(|error| format!("AudioDeviceCreateIOProcIDWithBlock failed: {error:?}"))
    }

    fn start(&mut self, proc: Self::Proc) -> Result<(), String> {
        // SAFETY: the registered callback and all native dependencies are owned.
        check(
            unsafe { AudioDeviceStart(self.device().0.0, Some(proc)) },
            "AudioDeviceStart",
        )
    }
    fn stop(&mut self, proc: Self::Proc) -> Result<(), String> {
        // SAFETY: this ID belongs to this live device and registration.
        check(
            unsafe { AudioDeviceStop(self.device().0.0, Some(proc)) },
            "AudioDeviceStop",
        )
    }
    fn destroy_proc(&mut self, proc: Self::Proc) -> Result<(), String> {
        // SAFETY: called once, before destroying its device/block/queue.
        check(
            unsafe { AudioDeviceDestroyIOProcID(self.device().0.0, proc) },
            "AudioDeviceDestroyIOProcID",
        )
    }
    fn drain(&mut self) {
        // This runs on the control owner, never on the callback's serial queue.
        // Unregistration precedes the barrier, so no later callback can enqueue.
        if let Some(queue) = &self.queue {
            queue.sync(|| ());
        }
    }
    fn release_device(&mut self) -> Result<(), String> {
        if let Some(watcher) = &mut self.input_format {
            // A failed removal retains the entire capture bundle and process
            // fence; a successful removal drains the listener queue before
            // releasing its callback or publishing full cleanup.
            watcher.close()?;
        }
        match self.device {
            None | Some(DeviceOwner::Physical(_)) => Ok(()),
            Some(DeviceOwner::Aggregate(_)) => {
                // SAFETY: IOProc is destroyed and drained before its aggregate.
                check(
                    unsafe { AudioHardwareDestroyAggregateDevice(self.device().0.0) },
                    "AudioHardwareDestroyAggregateDevice",
                )
            }
        }
    }
    fn retain(&mut self, failure: &Failure) {
        let slot = match self.device {
            Some(DeviceOwner::Physical(_)) => &MIC_FAILURE,
            None | Some(DeviceOwner::Aggregate(_)) => &SYSTEM_FAILURE,
        };
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(failure.clone());
    }
    fn release_tap(&mut self) -> Result<(), String> {
        if let Some(tap) = &self.tap {
            let id: &ca::Obj = tap;
            // SAFETY: the referencing aggregate has been destroyed first.
            check(
                unsafe { AudioHardwareDestroyProcessTap(id.0) },
                "AudioHardwareDestroyProcessTap",
            )
        } else {
            Ok(())
        }
    }
}
#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioDeviceStart(device: u32, proc: Option<DeviceIoProcId>) -> i32;
    fn AudioDeviceStop(device: u32, proc: Option<DeviceIoProcId>) -> i32;
    fn AudioDeviceDestroyIOProcID(device: u32, proc: DeviceIoProcId) -> i32;
    fn AudioHardwareDestroyAggregateDevice(device: u32) -> i32;
    fn AudioHardwareDestroyProcessTap(tap: u32) -> i32;
}
