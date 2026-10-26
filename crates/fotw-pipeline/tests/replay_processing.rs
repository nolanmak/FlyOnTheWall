use fotw_pipeline::replay::{ReplaySource, ReplayWriter};

#[test]
fn original_input_keeps_the_existing_header_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let _owner = ReplayWriter::create(dir.path(), "fixture", ReplaySource::Mic, 0).unwrap();
    let bytes = std::fs::read(dir.path().join("mic.stt16")).unwrap();
    assert_eq!(&bytes[..8], b"FOTWRPL1");
    let size = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    assert_eq!(
        &bytes[12..12 + size],
        br#"{"session_id":"fixture","source":"mic","sample_rate":16000,"first_sample":0}"#
    );
}

#[test]
fn processing_validation_refusal_precedes_torn_sample_repair() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = ReplayWriter::create_with_processing(
        dir.path(),
        "fixture",
        ReplaySource::Mic,
        160,
        &"a".repeat(64),
    )
    .unwrap();
    owner.append(&[11, 22, 33]).unwrap();
    owner.sync().unwrap();
    drop(owner);
    let path = dir.path().join("mic.stt16");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.push(0x7f);
    std::fs::write(&path, &bytes).unwrap();
    let result =
        ReplayWriter::resume_validated(dir.path(), "fixture", ReplaySource::Mic, |reader| {
            assert_eq!(reader.processing_id(), Some("a".repeat(64).as_str()));
            Err(std::io::ErrorKind::InvalidData.into())
        });
    assert!(result.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(
        ReplayWriter::resume_prefix_validated(dir.path(), "fixture", ReplaySource::Mic, |_| Err(
            std::io::ErrorKind::InvalidData.into()
        ))
        .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
