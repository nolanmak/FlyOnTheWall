//! Promotion: a finished session becomes durable media (§9.2, §5.4).
//!
//! §9.2 names two locations and the difference between them is the whole
//! point:
//!
//! ```text
//! <root>/sessions/<id>/                       live WAL session dirs (§5.4)
//! <root>/media/<yyyy>/<mm>/<meeting_id>/      the durable archive
//! ```
//!
//! A session directory is shaped for a crash *during* the meeting: headerless
//! PCM whose length is its own header, a manifest whose missing `ended_at_ms`
//! is the recovery signal, an append-only JSONL. None of that is how you want
//! to keep audio for a year. The media tree is shaped for the opposite job —
//! addressable by meeting, cheap to sweep, and the thing every `rel_path` in
//! the database points at.
//!
//! Nothing moved a session between them. Until this module existed the audio
//! stayed in `sessions/` forever, `media/` was never created, and the §9.5
//! retention engine had nothing to look at, which is why it could sit there
//! for months without anyone noticing it never ran.
//!
//! # The sequence, and what a crash at each point costs
//!
//! | step | what it does | crash here costs |
//! |---|---|---|
//! | [`claim`] | records the meeting id and destination in the manifest | nothing; re-claimable |
//! | [`encode`] | PCM → Opus, in the session dir | the transcode; PCM is untouched |
//! | [`publish`] | Opus → `media/…/<name>.part` → `rename` | a `.part` file |
//! | [`stamp`] | records the published tracks in the manifest | a repeat of `publish` |
//! | [`retire`] | verifies, then removes the session dir | a `.retired` directory |
//!
//! Two invariants hold at every row of that table.
//!
//! **The session directory is the only copy until the media provably is one.**
//! It is removed last, by [`retire`], and only after every published file has
//! been re-stat'd and found at the exact byte length that was recorded. Not
//! "the rename returned `Ok` earlier" — the file, now, at the size it should
//! be. A missing or truncated track aborts the retire and leaves the session
//! where it is, which turns the worst case from "the meeting is gone" into
//! "the disk did not get smaller".
//!
//! **A half-written Opus file never wears a finished name.** Bytes land under
//! `<name>.part` and reach `<name>` by `rename(2)`, which is atomic within a
//! filesystem. A reader — the retention sweeper, the player, an export —
//! therefore never sees a truncated stream under a name that says it is whole.
//! Writing straight to the final name and hoping the process survives is the
//! defect this design exists to preclude; it fails silently, months later, as
//! a meeting that will not play.
//!
//! Every step is idempotent and [`promote`] is resumable, so the recovery
//! procedure after any crash is "run it again".

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::opus::OpusError;
use crate::wal::{self, EncodedTrack, LEGS, Manifest};

/// The media root's name under the data root (§9.2).
pub const MEDIA_DIR: &str = "media";

/// Suffix a track wears while it is still being written.
///
/// The one detail that makes the crash-safety claim true rather than
/// aspirational. See the module docs.
pub const PART_SUFFIX: &str = ".part";

/// Suffix a session directory wears between "provably superseded" and "gone".
///
/// `remove_dir_all` is not atomic: it can delete `manifest.json` and then die,
/// leaving a directory that no longer identifies itself. A rename *is* atomic,
/// so renaming first turns that ambiguous leftover into an unambiguous one —
/// anything wearing this suffix is garbage and can be removed on sight.
pub const RETIRED_SUFFIX: &str = ".retired";

/// The destination a session was assigned, recorded before any bytes move.
///
/// Written by [`claim`] and never recomputed. Deciding the path once means a
/// crash cannot land the two halves of one meeting in two different
/// directories because the date arithmetic saw a different `started_at_ms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// The meeting this session became.
    pub meeting_id: String,
    /// `media/<yyyy>/<mm>/<meeting_id>`, relative to the data root.
    pub rel_dir: String,
}

/// One promoted Opus track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotedTrack {
    /// `mic` or `system` — the `recordings.channel` column of §9.3.
    pub channel: String,
    /// Path **relative to the data root**. §9.7 invariant 5 forbids absolute
    /// paths in the database precisely so the library can be moved or restored
    /// onto another machine, and this is the value that goes into
    /// `recordings.rel_path`.
    pub rel_path: String,
    /// Size on disk, and the number [`retire`] verifies against before it
    /// removes the only other copy.
    pub bytes: u64,
    /// Duration of the encoded audio.
    pub duration_ms: u64,
    /// Rate the Opus stream was encoded at.
    pub sample_rate_hz: u32,
}

/// A session's audio, published into the media tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Promotion {
    /// The meeting it belongs to.
    pub meeting_id: String,
    /// `media/<yyyy>/<mm>/<meeting_id>`, relative to the data root.
    pub rel_dir: String,
    /// The tracks, one per channel.
    pub tracks: Vec<PromotedTrack>,
}

impl Promotion {
    /// Total bytes the media tree now holds for this meeting.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.tracks.iter().map(|t| t.bytes).sum()
    }
}

/// Why a promotion could not be completed.
///
/// Every variant leaves the session directory in place. There is deliberately
/// no error path that removes it.
#[derive(Debug, thiserror::Error)]
pub enum PromoteError {
    /// No session directory, or no manifest in it.
    #[error("no session at {dir}")]
    NoSession {
        /// Where we looked.
        dir: PathBuf,
    },
    /// The session has no meeting id, so there is nowhere to put it.
    #[error("session {dir} has not been claimed by a meeting")]
    Unclaimed {
        /// The session.
        dir: PathBuf,
    },
    /// The session is still being written (§5.4: `ended_at_ms` absent).
    #[error("session {dir} has not been finalized; it may still be recording")]
    Unfinalized {
        /// The session.
        dir: PathBuf,
    },
    /// [`publish`] was called before [`encode`].
    #[error("session {dir} has no Opus tracks yet")]
    NotEncoded {
        /// The session.
        dir: PathBuf,
    },
    /// [`retire`] was called before [`stamp`].
    #[error("session {dir} has not been published")]
    NotPublished {
        /// The session.
        dir: PathBuf,
    },
    /// A published track is missing or the wrong size, so the session
    /// directory is still the only copy and stays put.
    #[error("{path} is {got:?} bytes, not the {want} that were published")]
    Unverified {
        /// The track that failed.
        path: PathBuf,
        /// Bytes recorded at publish time.
        want: u64,
        /// Bytes found now, or `None` if the file is gone.
        got: Option<u64>,
    },
    /// The transcode failed.
    #[error("encoding {dir}: {source}")]
    Encode {
        /// The session.
        dir: PathBuf,
        /// The underlying failure.
        source: OpusError,
    },
    /// Underlying I/O, with the operation and path that failed.
    #[error("{what} {path}: {source}")]
    Io {
        /// What was being attempted.
        what: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The OS error.
        source: std::io::Error,
    },
}

fn io(what: &'static str, path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> PromoteError {
    let path = path.into();
    move |source| PromoteError::Io { what, path, source }
}

/// `media/<yyyy>/<mm>/<meeting_id>` for a meeting that started at `started_at_ms`.
///
/// UTC, and deliberately so: the layout is an index, not a display. A local
/// date would put the same meeting in two different directories depending on
/// where the laptop was when it was archived, which is exactly the sort of
/// thing that makes a file impossible to find two years later.
#[must_use]
pub fn media_rel_dir(started_at_ms: u64, meeting_id: &str) -> PathBuf {
    let (y, m, _) = civil_from_ms(started_at_ms);
    PathBuf::from(MEDIA_DIR)
        .join(format!("{y:04}"))
        .join(format!("{m:02}"))
        .join(meeting_id)
}

/// Record which meeting this session became, and where its audio will live.
///
/// Called once the meeting row exists, so the id is real. Writing it into the
/// manifest is what makes promotion resumable without a database: a crash
/// leaves a session directory that still knows its own destination.
///
/// **Call this after [`crate::wal::SessionWal::finalize`], never before.** A
/// live `SessionWal` holds the manifest in memory and rewrites the whole
/// document on `finalize` and on every `mark_gap`, so a claim written while
/// the session is still open would be silently overwritten by the recorder.
/// Everything downstream of finalize — [`encode`], [`publish`], [`stamp`] —
/// reads the manifest back off disk first and therefore preserves it.
///
/// # Errors
///
/// Propagates failures reading or rewriting the manifest.
pub fn claim(
    session_dir: impl AsRef<Path>,
    meeting_id: &str,
    started_at_ms: u64,
) -> Result<Claim, PromoteError> {
    let dir = session_dir.as_ref();
    let mut manifest = manifest_of(dir)?;
    let claim = Claim {
        meeting_id: meeting_id.to_owned(),
        rel_dir: media_rel_dir(started_at_ms, meeting_id)
            .to_string_lossy()
            .into_owned(),
    };
    manifest.claim = Some(claim.clone());
    wal::write_manifest_at(dir, &manifest).map_err(io("rewriting the manifest in", dir))?;
    Ok(claim)
}

/// Step 2: transcode the PCM legs to Opus, in place, leaving the PCM alone.
///
/// A no-op if the manifest already records an encode, which is what makes a
/// resumed promotion cheap rather than a second pass over 173 MB of PCM.
///
/// # Errors
///
/// [`PromoteError::Encode`] if libopus or the container fails.
pub fn encode(session_dir: impl AsRef<Path>) -> Result<(), PromoteError> {
    let dir = session_dir.as_ref();
    let manifest = manifest_of(dir)?;
    if manifest.encoded.is_some() {
        return Ok(());
    }
    wal::encode_session(dir).map_err(|source| PromoteError::Encode {
        dir: dir.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Step 3: copy the finished Opus tracks into the media tree.
///
/// Each track is written to `<name>.part`, `fsync`'d, and then `rename`d onto
/// its final name, so no reader ever sees a partial file under a name that
/// claims to be whole. Copies rather than moves: the session directory has to
/// remain a complete copy until [`retire`] has verified this one.
///
/// # Errors
///
/// [`PromoteError::Unfinalized`], [`PromoteError::Unclaimed`] or
/// [`PromoteError::NotEncoded`] if the session is not ready, and
/// [`PromoteError::Io`] for anything the filesystem refuses.
pub fn publish(
    session_dir: impl AsRef<Path>,
    data_root: impl AsRef<Path>,
) -> Result<Promotion, PromoteError> {
    let dir = session_dir.as_ref();
    let root = data_root.as_ref();
    let manifest = manifest_of(dir)?;
    let claim = ready_claim(dir, &manifest)?;
    let encoded = manifest
        .encoded
        .as_ref()
        .ok_or_else(|| PromoteError::NotEncoded {
            dir: dir.to_path_buf(),
        })?;

    let dest = root.join(&claim.rel_dir);
    std::fs::create_dir_all(&dest).map_err(io("creating the media directory", &dest))?;

    let mut tracks = Vec::with_capacity(LEGS.len());
    for (channel, track) in [("system", &encoded.system), ("mic", &encoded.mic)] {
        tracks.push(publish_one(dir, &dest, &claim.rel_dir, channel, track)?);
    }
    // One directory sync for the whole meeting rather than one per track: the
    // renames are already durable-in-order, and what this pins is the
    // directory entries themselves against a power cut.
    sync_dir(&dest);

    Ok(Promotion {
        meeting_id: claim.meeting_id,
        rel_dir: claim.rel_dir,
        tracks,
    })
}

/// Step 4: record the published tracks in the manifest.
///
/// The commit point. Before it, a crash means "publish again"; after it, a
/// crash means "verify and retire". Written through the manifest's atomic
/// tmp-then-rename path, so the record itself cannot be torn.
///
/// # Errors
///
/// Propagates failures rewriting the manifest.
pub fn stamp(session_dir: impl AsRef<Path>, promotion: &Promotion) -> Result<(), PromoteError> {
    let dir = session_dir.as_ref();
    let mut manifest = manifest_of(dir)?;
    manifest.promoted = Some(promotion.clone());
    wal::write_manifest_at(dir, &manifest).map_err(io("rewriting the manifest in", dir))
}

/// Step 5: verify the published media, then remove the session directory.
///
/// Returns the bytes reclaimed. **This is the only destructive step**, and the
/// verification in front of it is what separates "reclaim space" from "lose
/// the meeting" — the same guard, and the same reasoning, as
/// [`crate::wal::discard_pcm`]'s refusal to unlink PCM the manifest does not
/// record an encode for.
///
/// # Errors
///
/// [`PromoteError::Unverified`] if any published track is missing or is not
/// the length that was published. The session directory is left alone.
pub fn retire(
    session_dir: impl AsRef<Path>,
    data_root: impl AsRef<Path>,
) -> Result<u64, PromoteError> {
    let dir = session_dir.as_ref();
    let root = data_root.as_ref();
    let manifest = manifest_of(dir)?;
    let promoted = manifest
        .promoted
        .as_ref()
        .ok_or_else(|| PromoteError::NotPublished {
            dir: dir.to_path_buf(),
        })?;
    verify(root, promoted)?;

    let freed = dir_bytes(dir);
    // Rename first: `remove_dir_all` can die half-way and leave a directory
    // that no longer has a manifest to identify itself with, which a later
    // scan cannot tell from a live session. A rename cannot.
    let retired = sibling(dir, RETIRED_SUFFIX);
    std::fs::rename(dir, &retired).map_err(io("retiring the session directory", dir))?;
    std::fs::remove_dir_all(&retired).map_err(io("removing the retired session", &retired))?;
    Ok(freed)
}

/// Run the whole sequence, skipping whatever is already done.
///
/// Idempotent and resumable: after a crash at any point, calling this again
/// converges. It re-publishes rather than trusting the previous run's
/// `rename`, because re-copying a few megabytes is cheap and the alternative —
/// believing a record of work instead of the work — is how a library ends up
/// with rows pointing at files that are not there.
///
/// # Errors
///
/// Any [`PromoteError`]. The session directory always survives a failure.
pub fn promote(
    session_dir: impl AsRef<Path>,
    data_root: impl AsRef<Path>,
) -> Result<Promotion, PromoteError> {
    let dir = session_dir.as_ref();
    let root = data_root.as_ref();

    let manifest = manifest_of(dir)?;
    ready_claim(dir, &manifest)?;

    // Already published and still intact? Then all that is left is the retire
    // that the last run did not reach.
    if let Some(done) = manifest.promoted.clone()
        && verify(root, &done).is_ok()
    {
        retire(dir, root)?;
        return Ok(done);
    }

    encode(dir)?;
    let promotion = publish(dir, root)?;
    stamp(dir, &promotion)?;
    retire(dir, root)?;
    Ok(promotion)
}

/// Every finalized, claimed session under `sessions_root` still awaiting
/// promotion.
///
/// A session that is not finalized is deliberately absent: §5.4 makes the
/// missing `ended_at_ms` the recovery signal, and a session that is still
/// being written is one the pump still owns. Promoting it would race the
/// recorder for its own PCM.
///
/// # Errors
///
/// Propagates a failure to read `sessions_root` itself. A single unreadable
/// session directory is skipped rather than failing the scan — one broken
/// session must not hide every other one.
pub fn pending(sessions_root: impl AsRef<Path>) -> std::io::Result<Vec<PathBuf>> {
    let root = sessions_root.as_ref();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || is_retired(&path) {
            continue;
        }
        let Ok(manifest) = wal::read_manifest(&path) else {
            continue;
        };
        if manifest.ended_at_ms.is_some() && manifest.claim.is_some() {
            out.push(path);
        }
    }
    Ok(out)
}

/// Finish every interrupted promotion found under `sessions_root`, and sweep
/// up any `.retired` leftovers.
///
/// This is what the daemon runs at startup. Returns one result per session so
/// a single failure is reported rather than aborting the rest: a session whose
/// disk went read-only must not stop the other nine from being archived.
#[must_use]
pub fn resume(
    sessions_root: impl AsRef<Path>,
    data_root: impl AsRef<Path>,
) -> Vec<Result<Promotion, PromoteError>> {
    let root = sessions_root.as_ref();
    let data_root = data_root.as_ref();
    sweep_retired(root);
    pending(root)
        .unwrap_or_default()
        .into_iter()
        .map(|dir| promote(&dir, data_root))
        .collect()
}

// --------------------------------------------------------------------- inner

fn manifest_of(dir: &Path) -> Result<Manifest, PromoteError> {
    wal::read_manifest(dir).map_err(|_| PromoteError::NoSession {
        dir: dir.to_path_buf(),
    })
}

/// The claim, once the session is in a state where promotion is allowed.
fn ready_claim(dir: &Path, manifest: &Manifest) -> Result<Claim, PromoteError> {
    if manifest.ended_at_ms.is_none() {
        return Err(PromoteError::Unfinalized {
            dir: dir.to_path_buf(),
        });
    }
    manifest
        .claim
        .clone()
        .ok_or_else(|| PromoteError::Unclaimed {
            dir: dir.to_path_buf(),
        })
}

fn publish_one(
    session_dir: &Path,
    dest: &Path,
    rel_dir: &str,
    channel: &str,
    track: &EncodedTrack,
) -> Result<PromotedTrack, PromoteError> {
    let src = session_dir.join(&track.file);
    let final_name = format!("{channel}.opus");
    let part = dest.join(format!("{final_name}{PART_SUFFIX}"));
    let final_path = dest.join(&final_name);

    let bytes = copy_durably(&src, &part)?;
    std::fs::rename(&part, &final_path).map_err(io("publishing the track to", &final_path))?;

    Ok(PromotedTrack {
        channel: channel.to_owned(),
        // Built with `/` rather than by joining OS paths, because this string
        // goes into the database and has to read the same on every platform
        // that ever opens the library.
        rel_path: format!("{rel_dir}/{final_name}"),
        bytes,
        duration_ms: track.duration_ms,
        sample_rate_hz: track.sample_rate_hz,
    })
}

/// Copy `src` to `dst` and force it to the platter, returning the bytes
/// written.
///
/// `sync_all`, not `flush`: a rename onto a file whose contents are still only
/// in the page cache is durable against `kill -9` and not against a power cut,
/// and this is the one copy of the meeting that is about to become the only
/// one.
fn copy_durably(src: &Path, dst: &Path) -> Result<u64, PromoteError> {
    let mut from = BufReader::new(File::open(src).map_err(io("opening the encoded track", src))?);
    let file = File::create(dst).map_err(io("creating the media file", dst))?;
    let mut to = BufWriter::new(file);
    let n = std::io::copy(&mut from, &mut to).map_err(io("writing the media file", dst))?;
    let file = to.into_inner().map_err(|e| PromoteError::Io {
        what: "flushing the media file",
        path: dst.to_path_buf(),
        source: e.into_error(),
    })?;
    file.sync_all().map_err(io("syncing the media file", dst))?;
    Ok(n)
}

/// Re-stat every published track and insist it is exactly as long as recorded.
fn verify(data_root: &Path, promoted: &Promotion) -> Result<(), PromoteError> {
    for t in &promoted.tracks {
        let path = data_root.join(&t.rel_path);
        let got = std::fs::metadata(&path).ok().map(|m| m.len());
        if got != Some(t.bytes) {
            return Err(PromoteError::Unverified {
                path,
                want: t.bytes,
                got,
            });
        }
    }
    Ok(())
}

fn sync_dir(dir: &Path) {
    // Best effort. It upgrades durability from "survives a crash" to
    // "survives a power cut"; the correctness of the sequence does not depend
    // on it, and not every filesystem allows it.
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
}

fn sibling(dir: &Path, suffix: &str) -> PathBuf {
    let name = dir.file_name().map_or_else(
        || "session".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    dir.parent()
        .unwrap_or(Path::new("."))
        .join(format!("{name}{suffix}"))
}

fn is_retired(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().ends_with(RETIRED_SUFFIX))
}

fn sweep_retired(sessions_root: &Path) {
    let Ok(entries) = std::fs::read_dir(sessions_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && is_retired(&path) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

fn dir_bytes(dir: &Path) -> u64 {
    crate::retention::dir_bytes(dir).unwrap_or(0)
}

/// Year, month, day (UTC) for a Unix epoch millisecond.
///
/// Hand-rolled rather than pulled in with a date crate: the only question this
/// module ever asks a calendar is "which `yyyy/mm` directory", and the
/// civil-from-days algorithm that answers it is fifteen lines. The version
/// here is Howard Hinnant's, which is exact for the whole proleptic Gregorian
/// range rather than only for the years someone remembered to test.
fn civil_from_ms(ms: u64) -> (i64, u32, u32) {
    civil_from_days((ms / 86_400_000) as i64)
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01 so leap day lands at the end of the year
    // and every month has a closed-form length.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_calendar_arithmetic_matches_known_dates() {
        // Epoch, a leap day, a year boundary, and the date this module was
        // written. A month that is off by one only shows up as audio filed
        // under the wrong directory, which nothing ever notices.
        assert_eq!(civil_from_ms(0), (1970, 1, 1));
        assert_eq!(civil_from_ms(951_782_400_000), (2000, 2, 29));
        assert_eq!(civil_from_ms(946_684_799_000), (1999, 12, 31));
        assert_eq!(civil_from_ms(946_684_800_000), (2000, 1, 1));
        assert_eq!(civil_from_ms(1_786_579_200_000), (2026, 8, 13));
    }

    #[test]
    fn the_media_path_zero_pads_the_month() {
        assert_eq!(
            media_rel_dir(1_767_225_600_000, "m").to_string_lossy(),
            "media/2026/01/m"
        );
    }
}
