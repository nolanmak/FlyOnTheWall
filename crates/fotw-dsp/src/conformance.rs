//! Producer-owned PCM contracts available to consumer test targets.
use crate::resample::{Downmixer, Resampler16k};

/// Callback boundaries must not change channel order, filter phase or PCM.
pub fn streaming() {
    for rate in [8_000, 16_000, 24_000, 44_100, 48_000, 96_000] {
        for channels in [1u16, 2] {
            let input: Vec<f32> = (0..rate / 2)
                .flat_map(|i| {
                    (0..channels).map(move |ch| {
                        let phase = i as f32 * 2.0 * std::f32::consts::PI * 997.0 / rate as f32;
                        phase.sin() * if ch == 0 { 0.5 } else { -0.25 }
                    })
                })
                .collect();
            let expected = Resampler16k::new(rate, channels)
                .unwrap()
                .process_all(&input)
                .unwrap();
            let mut resampler = Resampler16k::new(rate, channels).unwrap();
            let mut streamed = Vec::new();
            // Deliberately split stereo frames as well as filter chunks.
            for chunk in input.chunks(137) {
                streamed.extend(resampler.process_all(chunk).unwrap());
            }
            assert_eq!(streamed, expected, "{rate} Hz, {channels} channels");
            assert!(streamed.iter().all(|v| v.is_finite()));
            assert_eq!(resampler.process_all(&[]).unwrap(), Vec::<f32>::new());
            let mono = Downmixer::to_mono(&streamed, channels);
            assert!((mono.len() as i64 - 8_000).abs() < 500);
            assert_eq!(Downmixer::to_le_bytes(&mono).len(), mono.len() * 2);
        }
    }
    assert_eq!(
        Downmixer::to_le_bytes(&[-2.0, 0.0, 2.0]),
        [1, 128, 0, 0, 255, 127]
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn streaming_contract() {
        super::streaming();
    }
}
