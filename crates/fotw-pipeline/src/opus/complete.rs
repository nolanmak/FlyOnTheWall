//! Strict streaming verification of complete mono archives written by this app.
//! Unlike the playable-prefix decoder, this requires a valid EOS and complete
//! framing. Storage is one Ogg page, one bounded packet and one codec frame.
use super::{
    DecoderHandle, GRANULE_RATE, MAX_FRAME_MS, MAX_PACKET_BYTES, OpusError, SUPPORTED_RATES, check,
    strerror, sys,
};
use std::io::Read;

/// Verified timing of a complete single-stream mono Opus archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompleteArchive {
    /// Informational encoder input rate recorded in OpusHead.
    pub input_rate_hz: u32,
    /// Intended PCM duration in 48 kHz granule ticks, excluding pre-skip.
    pub pcm_samples_48k: u64,
    /// Complete audio packets decoded without retaining their output.
    pub packets: u64,
    /// Entire validated file length; bytes after EOS are refused.
    pub bytes: u64,
}
impl CompleteArchive {
    /// Whole milliseconds of intended playback.
    pub fn duration_ms(&self) -> u64 {
        self.pcm_samples_48k / 48
    }
}
fn invalid(message: &str) -> OpusError {
    OpusError::Container(message.into())
}
const fn crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut value = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 0x80000000 != 0 {
                (value << 1) ^ 0x04c11db7
            } else {
                value << 1
            };
            bit += 1;
        }
        table[i] = value;
        i += 1;
    }
    table
}
const CRC: [u32; 256] = crc_table();
fn checksum<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> u32 {
    let mut value = 0;
    for part in parts {
        for byte in part {
            value = (value << 8) ^ CRC[((value >> 24) as u8 ^ byte) as usize];
        }
    }
    value
}
fn tags(packet: &[u8]) -> bool {
    if packet.len() < 16 || &packet[..8] != b"OpusTags" {
        return false;
    }
    let vendor = u32::from_le_bytes(packet[8..12].try_into().unwrap()) as usize;
    let Some(end) = 12usize.checked_add(vendor) else {
        return false;
    };
    // The app writes a vendor string and no user comments. Refuse unsupported
    // profiles rather than allocating arbitrary metadata during startup.
    packet.get(end..end.saturating_add(4)) == Some(&[0, 0, 0, 0][..])
        && end.checked_add(4) == Some(packet.len())
}

/// Validate a complete app-written archive with bounded memory, decoding every
/// packet through libopus. Chained/multiplexed/foreign profiles, malformed CRCs,
/// truncated data, unsupported packet sizes and missing EOS are errors.
/// This performs blocking work and must never run on a capture callback.
pub fn inspect_complete_archive<R: Read>(mut reader: R) -> Result<CompleteArchive, OpusError> {
    let mut body = vec![0u8; 65025];
    let mut packet = Vec::with_capacity(MAX_PACKET_BYTES);
    let mut frame = vec![0f32; GRANULE_RATE as usize / 1000 * MAX_FRAME_MS];
    let mut decoder = None;
    let mut serial = None;
    let mut sequence = 0u32;
    let mut packet_index = 0u64;
    let mut decoded = 0u64;
    let mut total_bytes = 0u64;
    let mut input_rate = 0;
    let mut skip = 0u64;
    let mut previous_granule = None;
    let mut ordinary_offset = None;
    loop {
        let mut header = [0u8; 27];
        if reader.read(&mut header[..1])? == 0 {
            return Err(invalid("archive has no complete end-of-stream"));
        }
        reader.read_exact(&mut header[1..])?;
        if &header[..4] != b"OggS" || header[4] != 0 || header[5] & !7 != 0 || header[26] == 0 {
            return Err(invalid("invalid Ogg page header"));
        }
        let flags = header[5];
        let page_serial = u32::from_le_bytes(header[14..18].try_into().unwrap());
        let page_sequence = u32::from_le_bytes(header[18..22].try_into().unwrap());
        if page_sequence != sequence
            || serial.is_some_and(|s| s != page_serial)
            || (flags & 2 != 0) != serial.is_none()
        {
            return Err(invalid("archive stream identity or page sequence changed"));
        }
        if (flags & 1 != 0) == packet.is_empty() {
            return Err(invalid("archive packet continuation is inconsistent"));
        }
        serial = Some(page_serial);
        sequence = sequence.wrapping_add(1);
        let count = usize::from(header[26]);
        let mut laces = [0u8; 255];
        reader.read_exact(&mut laces[..count])?;
        let bytes: usize = laces[..count].iter().map(|n| usize::from(*n)).sum();
        reader.read_exact(&mut body[..bytes])?;
        let expected = u32::from_le_bytes(header[22..26].try_into().unwrap());
        header[22..26].fill(0);
        if checksum([&header[..], &laces[..count], &body[..bytes]]) != expected {
            return Err(invalid("archive Ogg page checksum mismatch"));
        }
        total_bytes = total_bytes
            .checked_add((27 + count + bytes) as u64)
            .ok_or_else(|| invalid("archive byte count overflow"))?;
        let before = packet_index;
        let mut offset = 0;
        for length in &laces[..count] {
            let n = usize::from(*length);
            if packet.len() + n > MAX_PACKET_BYTES {
                return Err(invalid("archive packet exceeds supported size"));
            }
            packet.extend_from_slice(&body[offset..offset + n]);
            offset += n;
            if *length == 255 {
                continue;
            }
            match packet_index {
                0 => {
                    if packet.len() != 19
                        || &packet[..8] != b"OpusHead"
                        || packet[8] != 1
                        || packet[9] != 1
                        || packet[16..18] != [0, 0]
                        || packet[18] != 0
                    {
                        return Err(invalid("archive is not the supported mono Opus profile"));
                    }
                    input_rate = u32::from_le_bytes(packet[12..16].try_into().unwrap());
                    if !SUPPORTED_RATES.contains(&input_rate) {
                        return Err(OpusError::UnsupportedRate(input_rate));
                    }
                    skip = u64::from(u16::from_le_bytes([packet[10], packet[11]]));
                    let mut error = 0;
                    // SAFETY: 48kHz mono is supported and error is a valid output pointer.
                    let raw =
                        unsafe { sys::opus_decoder_create(GRANULE_RATE as i32, 1, &mut error) };
                    check("opus_decoder_create", error)?;
                    if raw.is_null() {
                        return Err(invalid("archive decoder allocation failed"));
                    }
                    decoder = Some(DecoderHandle(raw));
                }
                1 => {
                    if !tags(&packet) {
                        return Err(invalid("archive has an unsupported comment header"));
                    }
                }
                _ => {
                    if packet.is_empty() {
                        return Err(invalid("archive contains an empty audio packet"));
                    }
                    let decoder = decoder
                        .as_ref()
                        .ok_or_else(|| invalid("archive has no decoder header"))?;
                    // SAFETY: packet is bounded by MAX_PACKET_BYTES and frame holds
                    // libopus's maximum 120ms mono output at the fixed 48kHz rate.
                    let n = unsafe {
                        sys::opus_decode_float(
                            decoder.0,
                            packet.as_ptr(),
                            packet.len() as i32,
                            frame.as_mut_ptr(),
                            frame.len() as i32,
                            0,
                        )
                    };
                    if n < 0 {
                        return Err(OpusError::Codec {
                            op: "opus_decode_float",
                            code: n,
                            msg: strerror(n),
                        });
                    }
                    decoded = decoded
                        .checked_add(n as u64)
                        .ok_or_else(|| invalid("archive sample count overflow"))?;
                }
            }
            packet_index += 1;
            packet.clear();
        }
        let granule = u64::from_le_bytes(header[6..14].try_into().unwrap());
        if before < 2 {
            // OpusHead and the app's small OpusTags each occupy their own page.
            if packet_index != before + 1 || !packet.is_empty() || flags & 4 != 0 || granule != 0 {
                return Err(invalid("invalid archive header page"));
            }
            continue;
        }
        let eos = flags & 4 != 0;
        if packet_index == before {
            if eos || granule != u64::MAX {
                return Err(invalid("archive page has no completed packet"));
            }
            continue;
        }
        if granule == u64::MAX || previous_granule.is_some_and(|old| granule < old) {
            return Err(invalid("archive granule position regressed"));
        }
        previous_granule = Some(granule);
        if !eos {
            let delta = granule
                .checked_sub(decoded)
                .ok_or_else(|| invalid("archive page precedes decoded audio"))?;
            // Earlier app versions added pre-skip to ordinary pages. Accept that
            // consistent legacy profile without changing stored bytes; its EOS
            // still must contain enough decoded audio for the intended duration.
            if (delta != 0 && delta != skip) || ordinary_offset.is_some_and(|old| old != delta) {
                return Err(invalid("archive ordinary page clock is inconsistent"));
            }
            ordinary_offset = Some(delta);
            continue;
        }
        if !packet.is_empty() || granule < skip || granule > decoded {
            return Err(invalid("archive end is not backed by decoded audio"));
        }
        let pcm = granule - skip;
        if !pcm.is_multiple_of(GRANULE_RATE / u64::from(input_rate)) {
            return Err(invalid(
                "archive end is not aligned to its encoder sample rate",
            ));
        }
        let mut trailing = [0];
        if reader.read(&mut trailing)? != 0 {
            return Err(invalid("archive has data after end-of-stream"));
        }
        return Ok(CompleteArchive {
            input_rate_hz: input_rate,
            pcm_samples_48k: pcm,
            packets: packet_index - 2,
            bytes: total_bytes,
        });
    }
}
