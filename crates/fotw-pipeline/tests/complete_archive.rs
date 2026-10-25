//! Complete archive validation must not accept the playable-prefix decoder's
//! deliberately permissive crash behavior, nor allocate a meeting's worth of PCM.
use fotw_pipeline::opus::{OpusOggWriter, inspect_complete_archive};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::Cursor;
thread_local! {
    static MEASURING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}
struct Counting;
fn count() {
    let _ = MEASURING.try_with(|active| {
        if active.get() {
            let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        }
    });
}
// SAFETY: every operation delegates its original pointer/layout to System.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static ALLOC: Counting = Counting;
fn archive(rate: u32, samples: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut writer = OpusOggWriter::with_page_interval(&mut out, rate, 40).unwrap();
    let frame = vec![0.2; 320];
    let mut left = samples;
    while left > 0 {
        let n = left.min(frame.len());
        writer.push_f32(&frame[..n]).unwrap();
        left -= n;
    }
    writer.finish().unwrap();
    out
}
#[test]
fn complete_duration_is_verified_at_every_supported_rate_including_empty_and_partial_tails() {
    for rate in [8000, 12000, 16000, 24000, 48000] {
        for n in [0, 1, rate as usize / 50 - 1, rate as usize + 17] {
            let bytes = archive(rate, n);
            let result = inspect_complete_archive(Cursor::new(&bytes)).unwrap();
            assert_eq!(result.input_rate_hz, rate);
            assert_eq!(result.pcm_samples_48k, n as u64 * (48000 / u64::from(rate)));
            assert_eq!(result.bytes, bytes.len() as u64);
            assert!(result.packets > 0);
        }
    }
}
#[test]
fn torn_corrupt_and_concatenated_archives_never_certify_completion() {
    let bytes = archive(16000, 16000);
    for cut in [0, 1, 27, bytes.len() / 2, bytes.len() - 1] {
        assert!(inspect_complete_archive(Cursor::new(&bytes[..cut])).is_err());
    }
    let mut bad = bytes.clone();
    let n = bad.len();
    bad[n - 1] ^= 0x80;
    assert!(inspect_complete_archive(Cursor::new(bad)).is_err());
    let mut chained = bytes.clone();
    chained.extend_from_slice(&bytes);
    assert!(inspect_complete_archive(Cursor::new(chained)).is_err());
}
fn measured(bytes: &[u8]) -> usize {
    ALLOCATIONS.with(|n| n.set(0));
    MEASURING.with(|on| on.set(true));
    let result = inspect_complete_archive(Cursor::new(bytes));
    MEASURING.with(|on| on.set(false));
    result.unwrap();
    ALLOCATIONS.with(Cell::get)
}
#[test]
fn validation_allocations_do_not_grow_with_decoded_duration() {
    let short = archive(16000, 1600);
    let long = archive(16000, 16000 * 20);
    let small = measured(&short);
    let large = measured(&long);
    assert!(
        large <= small + 1,
        "allocation count grew with pages/decoded output: {small} -> {large}"
    );
    assert!(
        large <= 8,
        "the verifier must keep only bounded reusable buffers"
    );
}
#[test]
fn complete_legacy_page_clocks_are_accepted_without_rewriting_archive_bytes() {
    let mut bytes = archive(16000, 16000);
    let skip = u16::from_le_bytes([bytes[38], bytes[39]]) as u64;
    let mut offset = 0;
    while offset < bytes.len() {
        let segments = bytes[offset + 26] as usize;
        let len = 27
            + segments
            + bytes[offset + 27..offset + 27 + segments]
                .iter()
                .map(|n| *n as usize)
                .sum::<usize>();
        let page = &mut bytes[offset..offset + len];
        let sequence = u32::from_le_bytes(page[18..22].try_into().unwrap());
        if sequence >= 2 && page[5] & 4 == 0 {
            let granule = u64::from_le_bytes(page[6..14].try_into().unwrap());
            page[6..14].copy_from_slice(&(granule + skip).to_le_bytes());
            fix_crc(page);
        }
        offset += len;
    }
    let observed = inspect_complete_archive(Cursor::new(&bytes)).unwrap();
    assert_eq!(observed.pcm_samples_48k, 48000);
    assert_eq!(observed.bytes, bytes.len() as u64);
}
#[test]
fn valid_checksums_do_not_hide_a_bad_sequence_or_invalid_codec_packet() {
    use ogg::writing::{PacketWriteEndInfo, PacketWriter};
    let source = archive(16000, 320);
    let mut input = ogg::reading::PacketReader::new(Cursor::new(source));
    let head = input.read_packet().unwrap().unwrap().data;
    let tags = input.read_packet().unwrap().unwrap().data;
    let mut out = Vec::new();
    let mut writer = PacketWriter::new(&mut out);
    writer
        .write_packet(head, 1, PacketWriteEndInfo::EndPage, 0)
        .unwrap();
    writer
        .write_packet(tags, 1, PacketWriteEndInfo::EndPage, 0)
        .unwrap();
    writer
        .write_packet(vec![3], 1, PacketWriteEndInfo::EndStream, 1272)
        .unwrap();
    drop(writer);
    assert!(
        inspect_complete_archive(Cursor::new(out)).is_err(),
        "a TOC needing a missing frame-count byte must fail actual decode"
    );
    let mut bytes = archive(16000, 16000);
    let first_page = 27
        + bytes[26] as usize
        + bytes[27..27 + bytes[26] as usize]
            .iter()
            .map(|x| *x as usize)
            .sum::<usize>();
    bytes[first_page + 18..first_page + 22].copy_from_slice(&9u32.to_le_bytes());
    fix_crc(&mut bytes[first_page..]);
    assert!(inspect_complete_archive(Cursor::new(bytes)).is_err());
}
fn fix_crc(bytes: &mut [u8]) {
    let segments = bytes[26] as usize;
    let len = 27
        + segments
        + bytes[27..27 + segments]
            .iter()
            .map(|x| *x as usize)
            .sum::<usize>();
    bytes[22..26].fill(0);
    let mut crc = 0u32;
    for byte in &bytes[..len] {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x80000000 != 0 {
                (crc << 1) ^ 0x04c11db7
            } else {
                crc << 1
            };
        }
    }
    bytes[22..26].copy_from_slice(&crc.to_le_bytes());
}
