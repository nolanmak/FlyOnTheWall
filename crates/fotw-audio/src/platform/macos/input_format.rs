//! Per-capture microphone format invalidation; callbacks only latch an atomic.
//! NativeAssets owns this before any registration. Its capture lifecycle calls
//! close explicitly and retains the whole bundle if removal fails; registered
//! callbacks must never be freed through an unconditional watcher Drop.
use cidre::{
    arc,
    core_audio::{Device, Obj, PropAddr, PropElement, PropListenerBlock, PropScope, PropSelector},
    dispatch,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct InputFormatWatch {
    block: arc::R<PropListenerBlock>,
    queue: arc::R<dispatch::Queue>,
    registered: Vec<(Obj, PropAddr)>,
}
impl InputFormatWatch {
    pub fn new(changed: Arc<AtomicBool>) -> Self {
        let queue = dispatch::Queue::serial_with_ar_pool();
        let block = cidre::blocks::EscBlock::new2(move |n: u32, addresses: *const PropAddr| {
            if n > 0 && !addresses.is_null() {
                changed.store(true, Ordering::Release);
            }
        });
        Self {
            block,
            queue,
            registered: Vec::new(),
        }
    }
    #[cfg(any(test, feature = "conformance"))]
    pub(super) fn notify_for_conformance(&mut self) {
        let address = PropSelector::DEVICE_NOMINAL_SAMPLE_RATE.global_addr();
        self.block.call(1, &address);
    }
    pub fn device_targets(device: Device) -> [(Obj, PropAddr); 3] {
        [
            (
                device.0,
                PropSelector::DEVICE_NOMINAL_SAMPLE_RATE.global_addr(),
            ),
            (
                device.0,
                PropSelector::DEVICE_STREAM_CFG.addr(PropScope::INPUT, PropElement::MAIN),
            ),
            (
                device.0,
                PropSelector::DEVICE_STREAMS.addr(PropScope::INPUT, PropElement::MAIN),
            ),
        ]
    }
    fn add_with(
        &mut self,
        target: (Obj, PropAddr),
        mut add: impl FnMut(
            Obj,
            &PropAddr,
            &dispatch::Queue,
            &mut PropListenerBlock,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        add(target.0, &target.1, &self.queue, &mut self.block)?;
        self.registered.push(target);
        Ok(())
    }
    pub fn add(&mut self, target: (Obj, PropAddr)) -> Result<(), String> {
        self.add_with(target, |object, address, queue, block| {
            object
                .add_prop_listener_block(address, Some(queue), block)
                .map_err(|e| format!("input format listener registration failed: {e:?}"))
        })
    }
    pub fn close(&mut self) -> Result<(), String> {
        self.close_with(|object, address, queue, block| {
            object
                .remove_prop_listener_block(address, Some(queue), block)
                .map_err(|e| format!("input format listener removal failed: {e:?}"))
        })
    }
    fn close_with(
        &mut self,
        mut remove: impl FnMut(
            Obj,
            &PropAddr,
            &dispatch::Queue,
            &mut PropListenerBlock,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut errors = Vec::new();
        self.registered.retain(|(object, address)| {
            if let Err(error) = remove(*object, address, &self.queue, &mut self.block) {
                errors.push(error);
                true
            } else {
                false
            }
        });
        self.queue.sync(|| ());
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> (Obj, PropAddr) {
        InputFormatWatch::device_targets(Device(Obj(77)))
            .into_iter()
            .next()
            .unwrap()
    }
    #[test]
    fn native_property_callback_latches_rate_changes_without_healing_or_crossing_generations() {
        let changed = Arc::new(AtomicBool::new(false));
        let fresh = Arc::new(AtomicBool::new(false));
        let mut watch = InputFormatWatch::new(changed.clone());
        let _next = InputFormatWatch::new(fresh.clone());
        let (_, address) = target();
        watch.block.call(1, &address);
        assert!(
            changed.load(Ordering::Acquire),
            "native rate notification must invalidate negotiated input"
        );
        assert!(!fresh.load(Ordering::Acquire));
        watch.block.call(0, std::ptr::null());
        assert!(changed.load(Ordering::Acquire));
    }
    #[test]
    fn partial_registration_is_owned_and_failed_removal_stays_owned() {
        let mut watch = InputFormatWatch::new(Arc::new(AtomicBool::new(false)));
        watch.add_with(target(), |_, _, _, _| Ok(())).unwrap();
        assert!(
            watch
                .add_with(target(), |_, _, _, _| Err("fixture add failure".into()))
                .is_err()
        );
        assert_eq!(watch.registered.len(), 1);
        assert!(
            watch
                .close_with(|_, _, _, _| Err("fixture remove failure".into()))
                .is_err()
        );
        assert_eq!(
            watch.registered.len(),
            1,
            "failed cleanup cannot claim native release"
        );
        watch.close_with(|_, _, _, _| Ok(())).unwrap();
        assert!(watch.registered.is_empty());
    }
    #[test]
    fn successful_removal_drains_already_queued_notifications_before_returning() {
        let changed = Arc::new(AtomicBool::new(false));
        let mut watcher = InputFormatWatch::new(changed.clone());
        watcher.add_with(target(), |_, _, _, _| Ok(())).unwrap();
        let mut block = watcher.block.clone();
        let (_, address) = target();
        watcher.queue.async_once(move || block.call(1, &address));
        watcher.close_with(|_, _, _, _| Ok(())).unwrap();
        assert!(
            changed.load(Ordering::Acquire),
            "cleanup must observe queued rate invalidation even without another IO callback"
        );
        assert!(watcher.registered.is_empty());
    }
    #[test]
    fn device_watch_targets_are_input_specific_except_the_shared_device_rate() {
        let targets = InputFormatWatch::device_targets(Device(Obj(77)));
        assert_eq!(targets[0].1.scope, PropScope::GLOBAL);
        assert_eq!(targets[1].1.scope, PropScope::INPUT);
        assert_eq!(targets[2].1.scope, PropScope::INPUT);
        assert!(targets.iter().all(|(object, _)| *object == Obj(77)));
    }
}
