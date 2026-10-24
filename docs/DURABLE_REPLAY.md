# Canonical transcription replay

`fotw_pipeline::replay` stores an append-only copy of the exact mono 16 kHz
PCM16 stream handed to transcription. It supplements the native-format capture
WAL. It does not implement automatic provider switching or enable any new
recording mode by itself. FlyOnTheWall #12 remains open.

## Integration contract

1. Create one `ReplayWriter` per source inside the session directory. Pass the
   WAL session identity and the source's absolute start sample on the 16 kHz
   session clock. Keep the raw capture WAL independent.
2. On the blocking pump, append after resampling, echo gating, and gap padding,
   before handing the same PCM to the provider. Suppressed audio is silence,
   never a missing interval. Surface derived-write failures without stopping
   original capture. An IO failure poisons the writer until recovery.
3. Read catchup on a blocking worker using a separate `ReplayReader`. Supply
   absolute sample positions, rather than accumulating rounded milliseconds.
   Each read is capped at 16,000 samples (one second). Readers see newly
   appended samples without sharing or moving the writer's file offset.
4. Before durably journaling provider coverage and its transcript payload,
   call `sync_through` for the acknowledged end sample. It rejects positions
   outside saved audio. This is an audio durability barrier, **not evidence
   that the provider completed transcription**. The controller must establish
   that independently and fence late results from earlier provider epochs.
5. After a crash, `ReplayWriter::resume` takes a nonblocking OS ownership lock,
   validates the identity and format, and truncates only a trailing half-sample
   before appending. Complete saved samples survive. Failed validation never
   rewrites the recording. Readers exclude a trailing half-sample.
6. The future failover integration must retain these files until all pending
   transcription is committed, including after Stop or a process crash.
   Existing session promotion retires its entire session directory; **do not
   enable the writer in recording sessions before that lifecycle is wired**.
   Explicit meeting deletion must delete the derived audio too.

The files are `system.stt16` and `mic.stt16`, each with a bounded, versioned
header followed by little-endian PCM16. A separate `.lock` file holds writer
ownership so Windows byte-range locking does not block audio readers. Creating
a second recording at the same source path refuses overwrite. No provider
credentials or configuration are stored.

At 32,000 bytes/second, this temporary copy costs 115.2 MB/hour per source.
Catchup memory stays bounded by the read limit, independently of outage length.
The production failover controller must also bound queued inference and cloud
commands; this file format alone does not bound either queue.

## Verification

`cargo test -p fotw-pipeline --test replay` checks a 73-second outage, a
nonzero source anchor, a committed cursor within a sample, independent live
readers and tracks, zero/repeated/full-range samples, bounded allocation,
overwrite and session mismatch refusal, sample-clock overflow, malformed
headers, restart after a torn sample, live writer exclusion, and the durable
checkpoint boundary. These are storage tests; they do not establish end-to-end
handoff, transcript arbitration, capture reliability, or model quality.
