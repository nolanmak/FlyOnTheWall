//! Archive retries must preserve their last audio samples and never replace a
//! retained complete archive with missing or truncated raw input.
use fotw_pipeline::{
    opus::decode_ogg_opus_file,
    wal::{SessionWal, TrackFormat, discard_pcm, encode_session},
};
fn captured(rate: u32, channels: u16) -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let mut wal =
        SessionWal::create_with_formats(root.path(), TrackFormat::new(rate, channels), None)
            .unwrap();
    let pcm: Vec<f32> = (0..rate as usize + 17)
        .flat_map(|n| {
            std::iter::repeat_n(
                0.4 * (std::f32::consts::TAU * 440.0 * n as f32 / rate as f32).sin(),
                channels as usize,
            )
        })
        .collect();
    wal.write_system(&pcm).unwrap();
    wal.checkpoint_capture(2000, true).unwrap();
    let dir = wal.finalize().unwrap();
    (root, dir)
}
#[test]
fn unsupported_input_rate_flushes_its_complete_resampler_tail_before_archive_retirement() {
    for rate in [11025, 22050, 44100, 96000] {
        let (_root, dir) = captured(rate, 2);
        let encoded = encode_session(&dir).unwrap();
        let decoded = decode_ogg_opus_file(dir.join("system.opus"), 16000).unwrap();
        let expected = ((u64::from(rate) + 17) * 16000 / u64::from(rate)) as usize;
        assert_eq!(decoded.samples.len(), expected);
        assert_eq!(encoded.system.duration_ms, expected as u64 * 1000 / 16000);
        let tail = &decoded.samples[expected - 160..];
        let rms = (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt();
        assert!(
            rms > 0.1,
            "the retained tail must contain audible source signal"
        );
        discard_pcm(&dir).unwrap();
        assert!(!dir.join("system.pcm").exists());
    }
}
#[test]
fn retry_with_missing_or_shorter_pcm_preserves_every_completed_archive_and_manifest() {
    for missing in [false, true] {
        let (_root, dir) = captured(16000, 1);
        encode_session(&dir).unwrap();
        let audio = std::fs::read(dir.join("system.opus")).unwrap();
        let mic = std::fs::read(dir.join("mic.opus")).unwrap();
        let manifest = std::fs::read(dir.join("manifest.json")).unwrap();
        if missing {
            discard_pcm(&dir).unwrap();
        } else {
            std::fs::OpenOptions::new()
                .write(true)
                .open(dir.join("system.pcm"))
                .unwrap()
                .set_len(100)
                .unwrap();
        }
        assert!(
            encode_session(&dir).is_err(),
            "retry may not overwrite complete audio when its original raw source is unavailable"
        );
        assert_eq!(std::fs::read(dir.join("system.opus")).unwrap(), audio);
        assert_eq!(std::fs::read(dir.join("mic.opus")).unwrap(), mic);
        assert_eq!(std::fs::read(dir.join("manifest.json")).unwrap(), manifest);
    }
}

#[test]
fn an_unavailable_second_source_is_detected_before_the_first_archive_is_opened() {
    for missing in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
        wal.write_system(&[0.2; 1600]).unwrap();
        wal.write_mic(&[0.4; 1600]).unwrap();
        wal.checkpoint_capture(200, true).unwrap();
        let dir = wal.finalize().unwrap();
        encode_session(&dir).unwrap();
        let system = std::fs::read(dir.join("system.opus")).unwrap();
        let mic = std::fs::read(dir.join("mic.opus")).unwrap();
        let manifest = std::fs::read(dir.join("manifest.json")).unwrap();
        if missing {
            std::fs::remove_file(dir.join("mic.pcm")).unwrap();
        } else {
            std::fs::OpenOptions::new()
                .write(true)
                .open(dir.join("mic.pcm"))
                .unwrap()
                .set_len(100)
                .unwrap();
        }
        assert!(encode_session(&dir).is_err());
        assert_eq!(std::fs::read(dir.join("system.opus")).unwrap(), system);
        assert_eq!(std::fs::read(dir.join("mic.opus")).unwrap(), mic);
        assert_eq!(std::fs::read(dir.join("manifest.json")).unwrap(), manifest);
    }
}

#[test]
fn an_orphan_mic_archive_is_preserved_when_its_raw_source_and_completion_metadata_are_missing() {
    let root = tempfile::tempdir().unwrap();
    let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
    wal.write_system(&[0.2; 1600]).unwrap();
    wal.write_mic(&[0.4; 1600]).unwrap();
    let dir = wal.finalize().unwrap();
    encode_session(&dir).unwrap();
    let path = dir.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest["encoded"] = serde_json::Value::Null;
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    std::fs::remove_file(dir.join("mic.pcm")).unwrap();
    let before = std::fs::read(dir.join("mic.opus")).unwrap();
    let system = std::fs::read(dir.join("system.opus")).unwrap();
    assert!(encode_session(&dir).is_err());
    assert_eq!(std::fs::read(dir.join("mic.opus")).unwrap(), before);
    assert_eq!(std::fs::read(dir.join("system.opus")).unwrap(), system);
}
#[test]
fn an_unrecorded_absent_microphone_without_an_archive_still_encodes_as_empty() {
    let (_root, dir) = captured(16000, 1);
    std::fs::remove_file(dir.join("mic.pcm")).unwrap();
    let encoded = encode_session(&dir).unwrap();
    assert_eq!(encoded.mic.pcm_bytes, 0);
    assert!(
        decode_ogg_opus_file(dir.join("mic.opus"), 16000)
            .unwrap()
            .samples
            .is_empty()
    );
}
#[cfg(unix)]
#[test]
fn a_linked_destination_does_not_overwrite_a_file_outside_the_capture() {
    let (root, dir) = captured(16000, 1);
    let sentinel = root.path().join("owned-sentinel");
    std::fs::write(&sentinel, b"owned fixture bytes").unwrap();
    std::os::unix::fs::symlink(&sentinel, dir.join("system.opus")).unwrap();
    assert!(encode_session(&dir).is_err());
    assert_eq!(std::fs::read(sentinel).unwrap(), b"owned fixture bytes");
    assert!(!dir.join("mic.opus").exists());
}
