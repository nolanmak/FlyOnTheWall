// Model-free checks of the diarization session (#246) and of the word timings
// `parakeet-unified` reports with `options.timings`: the logic around the
// models, with the model replaced by a closure.
import Foundation
import SpeechdCore

private func span(_ speaker: String, _ startMs: UInt64, _ endMs: UInt64) -> SpeakerSpan {
    SpeakerSpan(startMs: startMs, endMs: endMs, speaker: speaker)
}

private func segment(_ speaker: String, _ startMs: UInt64, _ endMs: UInt64) -> TimedSegment {
    TimedSegment(startMs: startMs, endMs: endMs, speaker: speaker)
}

func finalSegmentsAreSortedMergedAndRelabelledByFirstAppearance() throws {
    let spans = [
        span("7", 5000, 6000),
        span("3", 0, 1000),
        span("3", 1000, 2000), // adjacent to the one before: merged
        span("7", 2500, 3000),
        span("3", 2800, 4000), // overlaps speaker 7, not speaker 3's last run
        span("7", 5500, 7000), // overlaps speaker 7's last run: merged
    ]
    let got = DiarizationSegments.final(spans)
    try expect(got == [segment("S1", 0, 2000), segment("S2", 2500, 3000), segment("S1", 2800, 4000),
                       segment("S2", 5000, 7000)], "\(got)")
    try expect(DiarizationSegments.final([]).isEmpty, "no speech, no segments")
}

func spansAreClippedToTheirWindow() throws {
    let got = DiarizationSegments.clipped([
        span("a", 900, 1200), // starts before the window
        span("b", 1500, 2600), // ends after it
        span("c", 0, 1000), // wholly before it
        span("", 1100, 1300), // no speaker
        span("d", 1400, 1400), // empty
    ], startMs: 1000, endMs: 2000)
    try expect(got == [span("a", 1000, 1200), span("b", 1500, 2000)], "\(got)")
}

/// Records what the session asked the model, and answers with one speaker
/// spilling 500 ms past both ends of every window.
actor WindowLog {
    var windows: [(count: Int, start: Int)] = []
    func record(_ count: Int, _ start: Int) { windows.append((count, start)) }
}

func aSessionHoldsAtMostOneWindowAndAcknowledgesEach() async throws {
    let log = WindowLog()
    let session = WindowedDiarizationSession(model: "diarizer@test") { window, start in
        await log.record(window.count, start)
        let startMs = UInt64(start) * 1000 / 16_000
        let endMs = UInt64(start + window.count) * 1000 / 16_000
        return WindowResult(spans: [span("9", startMs > 500 ? startMs - 500 : 0, endMs + 500)], speakerCount: 1)
    }
    var events: [EngineEvent] = []
    for _ in 0..<5 {
        events += try await session.append([Float](repeating: 0.1, count: 70_000))
        try expect(session.bufferedSamples < WindowedDiarizationSession.windowSamples,
                   "buffered \(session.bufferedSamples)")
    }
    try expect(events == [.partial(text: "", startMs: 0, endMs: 10_000, utterance: nil),
                          .partial(text: "", startMs: 0, endMs: 20_000, utterance: nil)], "\(events)")
    // One frame of the largest size the protocol allows spans three windows.
    let big = try await session.append([Float](repeating: 0.1, count: 524_288))
    try expect(big.count == 3 && session.bufferedSamples < WindowedDiarizationSession.windowSamples,
               "\(big) \(session.bufferedSamples)")
    let done = try await session.finish()
    let total = 350_000 + 524_288
    let windows = await log.windows
    try expect(windows.dropLast().allSatisfy { $0.count == WindowedDiarizationSession.windowSamples },
               "full windows only before finish: \(windows.map(\.count))")
    try expect(windows.map(\.start) == stride(from: 0, to: total, by: 160_000).map { $0 },
               "\(windows.map(\.start))")
    try expect(windows.last?.count == total % 160_000, "the tail is processed on finish")
    let endMs = UInt64(total) * 1000 / 16_000
    try expect(done == [.final(text: "", startMs: 0, endMs: endMs, segments: [segment("S1", 0, endMs)],
                               model: "diarizer@test", utterance: nil)], "\(done)")
}

func aSessionPastTheSpeakerCapFailsWithAnEngineError() async throws {
    for (count, fails) in [(128, false), (129, true)] {
        let session = WindowedDiarizationSession(model: "m") { _, _ in
            WindowResult(spans: [], speakerCount: count)
        }
        do {
            _ = try await session.append([Float](repeating: 0, count: WindowedDiarizationSession.windowSamples))
            try expect(!fails, "\(count) speakers accepted")
        } catch let error as SpeechdError {
            try expect(fails && error.code == "engine", "\(count) speakers: \(error)")
        }
    }
}

func aSessionPastItsBoundFailsTooLong() async throws {
    let session = WindowedDiarizationSession(model: "m", maxAudioMs: 20_000) { _, _ in
        WindowResult(spans: [], speakerCount: 0)
    }
    _ = try await session.append([Float](repeating: 0, count: 320_000))
    do {
        _ = try await session.append([0])
        throw CheckFailure(description: "accepted audio past max_audio_ms")
    } catch let error as SpeechdError {
        try expect(error.code == "too_long", "\(error)")
    }
}

func sentencePiecesMergeIntoTimedWords() throws {
    let got = WordTimings.words([
        TimedPiece(text: "Hel", startSeconds: 0.0, endSeconds: 0.2), // first piece starts a word
        TimedPiece(text: "lo", startSeconds: 0.2, endSeconds: 0.4),
        TimedPiece(text: ",", startSeconds: 0.4, endSeconds: 0.45),
        TimedPiece(text: "\u{2581}wor", startSeconds: 0.5, endSeconds: 0.6),
        TimedPiece(text: "ld", startSeconds: 0.6, endSeconds: 0.9),
        TimedPiece(text: " again", startSeconds: 1.2, endSeconds: 1.5),
        TimedPiece(text: " ", startSeconds: 1.6, endSeconds: 1.7), // nothing to say
    ])
    try expect(got == [TimedSegment(startMs: 0, endMs: 450, text: "Hello,"),
                       TimedSegment(startMs: 500, endMs: 900, text: "world"),
                       TimedSegment(startMs: 1200, endMs: 1500, text: "again")], "\(got)")
    try expect(WordTimings.words([]).isEmpty, "no pieces, no words")
}

actor StateLog {
    var seen: [Int] = []
    func record(_ state: Int) { seen.append(state) }
}

func aSessionCarriesItsSpeakerStateFromWindowToWindow() async throws {
    let log = StateLog()
    let session = WindowedDiarizationSession(model: "m", initial: 0) { _, _, state in
        await log.record(state)
        return (WindowResult(spans: [], speakerCount: 1), state + 1)
    }
    _ = try await session.append([Float](repeating: 0, count: WindowedDiarizationSession.windowSamples * 3 + 5))
    _ = try await session.finish()
    let seen = await log.seen
    try expect(seen == [0, 1, 2, 3], "each window sees the previous window's state: \(seen)")
}

let diarizationCases: [(String, @Sendable () async throws -> Void)] = [
    ("diarization segments are sorted, merged and relabelled by first appearance",
     finalSegmentsAreSortedMergedAndRelabelledByFirstAppearance),
    ("diarization spans are clipped to their window", spansAreClippedToTheirWindow),
    ("a diarization session holds at most one window and acknowledges each",
     aSessionHoldsAtMostOneWindowAndAcknowledgesEach),
    ("a diarization session past 128 speakers fails with an engine error",
     aSessionPastTheSpeakerCapFailsWithAnEngineError),
    ("a diarization session past max_audio_ms fails too_long", aSessionPastItsBoundFailsTooLong),
    ("SentencePiece pieces merge into timed words", sentencePiecesMergeIntoTimedWords),
    ("a diarization session carries its speaker state from window to window",
     aSessionCarriesItsSpeakerStateFromWindowToWindow),
]
