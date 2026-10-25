//! Deletion must verify both replacement archives before unlinking either raw leg.
use fotw_pipeline::wal::{SessionWal, discard_pcm, encode_session};

fn encoded() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
    wal.write_system(&[0.2; 1600]).unwrap();
    wal.write_mic(&[0.4; 1600]).unwrap();
    wal.checkpoint_capture(200, true).unwrap();
    let dir = wal.finalize().unwrap();
    encode_session(&dir).unwrap();
    (root, dir)
}

#[test]
fn invalid_second_archive_preserves_both_raw_sources_and_manifest() {
    for case in ["missing", "crc", "truncated", "duration", "path"] {
        let (_root, dir) = encoded();
        let path = dir.join("mic.opus");
        match case {
            "missing" => std::fs::remove_file(path).unwrap(),
            "crc" => {
                let mut bytes = std::fs::read(&path).unwrap();
                let n = bytes.len();
                bytes[n - 1] ^= 1;
                std::fs::write(path, bytes).unwrap();
            }
            "truncated" => {
                let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
                f.set_len(f.metadata().unwrap().len() - 1).unwrap();
            }
            _ => {
                let path = dir.join("manifest.json");
                let mut m: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                if case == "path" {
                    m["encoded"]["mic"]["file"] = "../other.opus".into();
                } else {
                    m["encoded"]["mic"]["duration_ms"] = 1.into();
                }
                std::fs::write(path, serde_json::to_vec(&m).unwrap()).unwrap();
            }
        }
        let before: Vec<_> = ["system.pcm", "mic.pcm", "manifest.json"]
            .map(|name| (name, std::fs::read(dir.join(name)).unwrap()))
            .into();
        assert!(discard_pcm(&dir).is_err(), "{case} must retain raw audio");
        for (name, bytes) in before {
            assert_eq!(
                std::fs::read(dir.join(name)).unwrap(),
                bytes,
                "{case}: {name}"
            );
        }
    }
}

#[test]
fn raw_samples_written_after_the_encode_are_never_retired_as_already_archived() {
    use std::io::Write;
    let (_root, dir) = encoded();
    std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("mic.pcm"))
        .unwrap()
        .write_all(&42i16.to_le_bytes())
        .unwrap();
    let system = std::fs::read(dir.join("system.pcm")).unwrap();
    let mic = std::fs::read(dir.join("mic.pcm")).unwrap();
    assert!(discard_pcm(&dir).is_err());
    assert_eq!(std::fs::read(dir.join("system.pcm")).unwrap(), system);
    assert_eq!(std::fs::read(dir.join("mic.pcm")).unwrap(), mic);
}

#[test]
fn a_live_capture_cannot_be_encoded_or_have_its_audio_retired() {
    let root = tempfile::tempdir().unwrap();
    let mut wal = SessionWal::create(root.path(), 16000, 1).unwrap();
    wal.write_system(&[0.2; 1600]).unwrap();
    wal.flush().unwrap();
    let dir = wal.dir().to_owned();
    assert!(encode_session(&dir).is_err());
    assert!(!dir.join("system.opus").exists());
    drop(wal);
    let (_root, dir) = encoded();
    let _owner = fotw_pipeline::capture_clock::CaptureOwner::recover(&dir).unwrap();
    assert!(discard_pcm(&dir).is_err());
    assert!(dir.join("system.pcm").exists());
    assert!(dir.join("mic.pcm").exists());
}

fn published() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    fotw_pipeline::promote::Promotion,
) {
    let (root, dir) = encoded();
    fotw_pipeline::promote::claim(&dir, "owned-fixture", 1_786_579_200_000).unwrap();
    let promotion = fotw_pipeline::promote::publish(&dir, root.path()).unwrap();
    fotw_pipeline::promote::stamp(&dir, &promotion).unwrap();
    (root, dir, promotion)
}

#[test]
fn same_size_corrupt_published_audio_cannot_authorize_directory_retirement() {
    let (root, dir, promotion) = published();
    let path = root.path().join(&promotion.tracks[1].rel_path);
    let mut bytes = std::fs::read(&path).unwrap();
    let n = bytes.len();
    bytes[n - 1] ^= 1;
    std::fs::write(path, bytes).unwrap();
    let raw = std::fs::read(dir.join("system.pcm")).unwrap();
    assert!(fotw_pipeline::promote::retire(&dir, root.path()).is_err());
    assert_eq!(std::fs::read(dir.join("system.pcm")).unwrap(), raw);
}

#[test]
fn modern_capture_ownership_also_protects_directory_retirement() {
    let (root, dir, _) = published();
    let owner = fotw_pipeline::capture_clock::CaptureOwner::recover(&dir).unwrap();
    assert!(fotw_pipeline::promote::retire(&dir, root.path()).is_err());
    assert!(dir.join("system.pcm").exists());
    drop(owner);
    assert!(fotw_pipeline::promote::retire(&dir, root.path()).is_ok());
}
