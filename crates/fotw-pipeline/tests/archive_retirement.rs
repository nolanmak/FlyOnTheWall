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

#[cfg(unix)]
#[test]
fn an_io_error_reading_the_second_track_preserves_both_previous_archives_and_manifest() {
    use std::{io::Write, os::unix::fs::PermissionsExt};
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("Permission-denied fixture requires an unprivileged user");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
    wal.write_system(&[0.2; 1600]).unwrap();
    wal.write_mic(&[0.4; 1600]).unwrap();
    let dir = wal.finalize().unwrap();
    encode_session(&dir).unwrap();
    let system = std::fs::read(dir.join("system.opus")).unwrap();
    let mic = std::fs::read(dir.join("mic.opus")).unwrap();
    let manifest = std::fs::read(dir.join("manifest.json")).unwrap();
    // A retry has new retained audio. The first encoder can complete, but
    // opening the ordinary second source fails after metadata preflight.
    std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("system.pcm"))
        .unwrap()
        .write_all(&[0x12; 3200])
        .unwrap();
    let raw = std::fs::read(dir.join("system.pcm")).unwrap();
    let permissions = std::fs::metadata(dir.join("mic.pcm"))
        .unwrap()
        .permissions();
    std::fs::set_permissions(dir.join("mic.pcm"), std::fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(
        std::fs::File::open(dir.join("mic.pcm")).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    let result = encode_session(&dir);
    std::fs::set_permissions(dir.join("mic.pcm"), permissions).unwrap();
    assert!(
        result.is_err(),
        "fixture must fail during second-source I/O"
    );
    assert_eq!(
        std::fs::read(dir.join("system.opus")).unwrap(),
        system,
        "a failed retry must keep the prior playable first archive"
    );
    assert_eq!(std::fs::read(dir.join("mic.opus")).unwrap(), mic);
    assert_eq!(std::fs::read(dir.join("manifest.json")).unwrap(), manifest);
    assert_eq!(std::fs::read(dir.join("system.pcm")).unwrap(), raw);
    let encoded = encode_session(&dir).unwrap();
    assert_eq!(encoded.system.pcm_bytes, raw.len() as u64);
    assert_eq!(
        decode_ogg_opus_file(dir.join(&encoded.system.file), 16000)
            .unwrap()
            .samples
            .len(),
        3200
    );
    discard_pcm(&dir).unwrap();
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
fn a_manifest_write_failure_preserves_the_selected_generation_and_retry_commits_both_tracks() {
    use fotw_pipeline::wal::SessionState;
    let (_root, dir) = captured(16000, 1);
    encode_session(&dir).unwrap();
    let previous = encode_session(&dir).unwrap();
    let system = std::fs::read(dir.join(&previous.system.file)).unwrap();
    let mic = std::fs::read(dir.join(&previous.mic.file)).unwrap();
    let manifest = std::fs::read(dir.join("manifest.json")).unwrap();
    // Both encoders can finish; the actual manifest staging open then fails.
    std::fs::create_dir(dir.join("manifest.json.tmp")).unwrap();
    assert!(encode_session(&dir).is_err());
    assert_eq!(std::fs::read(dir.join("manifest.json")).unwrap(), manifest);
    assert_eq!(
        std::fs::read(dir.join(&previous.system.file)).unwrap(),
        system
    );
    assert_eq!(std::fs::read(dir.join(&previous.mic.file)).unwrap(), mic);
    assert_eq!(
        SessionState::read(&dir).unwrap().manifest.encoded,
        Some(previous.clone())
    );
    std::fs::remove_dir(dir.join("manifest.json.tmp")).unwrap();
    let next = encode_session(&dir).unwrap();
    assert_ne!(next.system.file, previous.system.file);
    assert_ne!(next.mic.file, previous.mic.file);
    assert_eq!(
        next.system.file.rsplit_once('/').unwrap().0,
        next.mic.file.rsplit_once('/').unwrap().0
    );
    assert_eq!(
        SessionState::read(&dir).unwrap().manifest.encoded,
        Some(next.clone())
    );
    discard_pcm(&dir).unwrap();
    let owner = fotw_pipeline::capture_clock::CaptureOwner::recover(&dir).unwrap();
    let clock = owner.snapshot().unwrap();
    assert_eq!(clock.system_tail_bytes, 0);
    assert_eq!(clock.mic_tail_bytes, 0);
    assert_eq!(
        std::fs::read(dir.join(&previous.system.file)).unwrap(),
        system
    );
}

#[cfg(unix)]
#[test]
fn a_linked_manifest_staging_path_cannot_overwrite_an_external_file() {
    let (root, dir) = captured(16000, 1);
    encode_session(&dir).unwrap();
    let manifest = std::fs::read(dir.join("manifest.json")).unwrap();
    let outside = root.path().join("external-sentinel");
    std::fs::write(&outside, b"retained outside bytes").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("manifest.json.tmp")).unwrap();
    assert!(
        encode_session(&dir).is_err(),
        "manifest staging must refuse links"
    );
    assert_eq!(std::fs::read(outside).unwrap(), b"retained outside bytes");
    assert_eq!(std::fs::read(dir.join("manifest.json")).unwrap(), manifest);
}

#[cfg(unix)]
#[test]
fn generated_archive_paths_reject_traversal_wrong_sources_and_linked_directories() {
    let (root, dir) = captured(16000, 1);
    encode_session(&dir).unwrap();
    let encoded = encode_session(&dir).unwrap();
    let original = std::fs::read(dir.join("manifest.json")).unwrap();
    let mut manifest: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let linked = dir.join(".archive-deadbeef");
    std::os::unix::fs::symlink(
        dir.join(encoded.system.file.rsplit_once('/').unwrap().0),
        &linked,
    )
    .unwrap();
    let outside = root.path().join("outside.opus");
    std::fs::write(&outside, b"outside sentinel").unwrap();
    for file in [
        "../outside.opus".to_owned(),
        encoded.mic.file.clone(),
        ".archive-deadbeef/system.opus".to_owned(),
    ] {
        manifest["encoded"]["system"]["file"] = file.into();
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(discard_pcm(&dir).is_err());
        assert!(dir.join("system.pcm").exists());
        assert!(dir.join("mic.pcm").exists());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside sentinel");
    }
    std::fs::write(dir.join("manifest.json"), original).unwrap();
    discard_pcm(&dir).unwrap();
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

#[cfg(unix)]
#[test]
fn the_encoder_open_itself_rejects_a_destination_link_created_after_preflight() {
    let root = tempfile::tempdir().unwrap();
    let sentinel = root.path().join("owned-sentinel");
    std::fs::write(&sentinel, b"owned fixture bytes").unwrap();
    let output = root.path().join("system.opus");
    assert!(std::fs::symlink_metadata(&output).is_err());
    std::os::unix::fs::symlink(&sentinel, &output).unwrap();
    assert!(fotw_pipeline::opus::OpusOggWriter::create(&output, 16000).is_err());
    assert_eq!(std::fs::read(sentinel).unwrap(), b"owned fixture bytes");
}
