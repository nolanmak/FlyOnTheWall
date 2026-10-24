use fotw_dsp::archive::ArchiveConverter;
fn tone(rate: u32, frames: usize, channels: usize) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| {
            let value = (std::f32::consts::TAU * 440.0 * i as f32 / rate as f32).sin() * 0.25;
            vec![value; channels]
        })
        .collect()
}
fn converted(input: &[f32], chunk: usize, rate: u32, channels: u16) -> Vec<f32> {
    let mut c = ArchiveConverter::new(rate, channels, 48_000, 2).unwrap();
    let mut out = Vec::new();
    for part in input.chunks(chunk) {
        out.extend(c.process(part).unwrap());
    }
    out.extend(c.finish().unwrap());
    out
}
#[test]
fn bluetooth_mono_becomes_exact_duration_stereo_without_chunk_boundary_loss() {
    let input = tone(16_000, 16_001, 1);
    let whole = converted(&input, input.len(), 16_000, 1);
    let split = converted(&input, 319, 16_000, 1);
    assert_eq!(split.len(), 48_003 * 2);
    assert_eq!(split, whole);
    assert!(split.chunks_exact(2).all(|f| f[0] == f[1]));
    let rms = (split.iter().map(|x| x * x).sum::<f32>() / split.len() as f32).sqrt();
    assert!(
        (0.16..0.19).contains(&rms),
        "conversion must retain the fixture signal: {rms}"
    );
}
#[test]
fn fractional_rate_and_partial_stereo_drains_preserve_duration_and_waveform() {
    let input = tone(44_100, 44_101, 2);
    let whole = converted(&input, input.len(), 44_100, 2);
    let split = converted(&input, 317, 44_100, 2);
    assert_eq!(split.len(), (44_101usize * 48_000 / 44_100) * 2);
    assert_eq!(split, whole);
}
#[test]
fn unchanged_format_is_bit_exact_and_finishing_is_idempotent() {
    let input = tone(48_000, 701, 2);
    let mut c = ArchiveConverter::new(48_000, 2, 48_000, 2).unwrap();
    let mut out = Vec::new();
    for part in input.chunks(319) {
        out.extend(c.process(part).unwrap());
    }
    out.extend(c.finish().unwrap());
    assert_eq!(out, input);
    assert!(c.finish().unwrap().is_empty());
    assert!(c.process(&[0.1, 0.1]).is_err());
}
#[test]
fn malformed_pcm_and_incomplete_final_frames_are_refused() {
    assert!(ArchiveConverter::new(0, 1, 48_000, 2).is_err());
    assert!(ArchiveConverter::new(48_000, 0, 48_000, 2).is_err());
    let mut c = ArchiveConverter::new(48_000, 2, 48_000, 2).unwrap();
    assert!(c.process(&[f32::NAN, 0.1]).is_err());
    c.process(&[0.1]).unwrap();
    assert!(c.finish().is_err());
}
#[test]
fn downsampling_filters_frequencies_above_target_nyquist() {
    let mut c = ArchiveConverter::new(48_000, 1, 16_000, 1).unwrap();
    let pcm: Vec<_> = (0..48_000)
        .map(|i| (std::f32::consts::TAU * 12_000.0 * i as f32 / 48_000.0).sin())
        .collect();
    let mut out = c.process(&pcm).unwrap();
    out.extend(c.finish().unwrap());
    assert_eq!(out.len(), 16_000);
    let middle = &out[1000..15000];
    let rms = (middle.iter().map(|x| x * x).sum::<f32>() / middle.len() as f32).sqrt();
    assert!(rms < 0.01, "aliasing energy {rms}");
}
