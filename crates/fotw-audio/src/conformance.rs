//! Device-independent capture-format contracts for downstream consumers.
use crate::{SampleFormat, StreamFormat};

/// Check channel framing, supported speech rates and recording-clock conversion.
/// On macOS, also drive the native format callback and IO guard with fixture PCM.
///
/// # Panics
/// If a contract changes. No audio device or system permission is used.
pub fn formats() {
    // The macOS consumer exercises the real block/IO guard with fixture PCM;
    // it never opens a capture device or registers a hardware listener.
    #[cfg(target_os = "macos")]
    crate::platform::macos::input_format_contract();
    for rate in [8_000, 16_000, 24_000, 44_100, 48_000, 96_000] {
        for channels in [1, 2] {
            let format = StreamFormat::new(rate, channels, SampleFormat::F32);
            assert!(format.is_plausible());
            assert_eq!(
                format.frames_in(rate as usize * channels as usize),
                Some(rate as usize)
            );
            assert_eq!(format.frames_to_ns(u64::from(rate)), 1_000_000_000);
        }
    }
    let stereo = StreamFormat::new(48_000, 2, SampleFormat::F32);
    assert_eq!(stereo.frames_in(3), None);
    assert!(!StreamFormat::new(0, 1, SampleFormat::F32).is_plausible());
    assert_eq!(
        StreamFormat::new(48_000, 0, SampleFormat::F32).frames_in(1),
        None
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn shared_capture_formats() {
        super::formats();
    }
}
