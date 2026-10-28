// The model-free half of a diarization stream (#246, docs/SPEECHD_PROTOCOL.md,
// "Engines"), and the word timings `parakeet-unified` reports with
// `options.timings`. The model is a closure here, so the logic around it is
// checked without one (Sources/SpeechdChecks/DiarizationChecks.swift), and
// `fake-diarizer` runs the same session with a model-free closure.
//
// Ported from Muesli (https://github.com/Muesli-HQ/muesli, MIT, © 2026 Pranav
// Hari), native/MuesliNative/Sources/MuesliNativeApp/MeetingRecordingTranscriber.swift
// at commit 97e455794112126a896cae17f5c829e8ffa820c8:
// `RecordedAudioDiarizationSession` (10 s windows, at most 128 speakers,
// `clippedSegments`) and `DiarizationPCMWindowReader` (at most one window of
// PCM held), with TranscriptionRuntime.swift `diarizeRecordedAudio` (~L1234)
// driving it window by window.
//
// Changed for the helper: audio arrives as PCM frames instead of from a file,
// so the session fills its one window from them; it acknowledges each
// processed window with a `partial` so the parent can pace itself under the
// server's 2-minute backlog; and on `finish` it merges and relabels the
// speakers into the protocol's `"S1"`, `"S2"`, … segments. The word merge
// follows FluidAudio's `buildWordTimings` (AsrTypes.swift, 0.17.3).
import Foundation

/// One speaker's turn, in ms from the stream's first sample, with the model's
/// own speaker id.
public struct SpeakerSpan: Sendable, Equatable {
    public let startMs: UInt64
    public let endMs: UInt64
    public let speaker: String
    public init(startMs: UInt64, endMs: UInt64, speaker: String) {
        self.startMs = startMs
        self.endMs = endMs
        self.speaker = speaker
    }
}

public enum DiarizationSegments {
    /// Muesli's `clippedSegments`: keep each span's part inside the window,
    /// and drop spans with no speaker or nothing left.
    public static func clipped(_ spans: [SpeakerSpan], startMs: UInt64, endMs: UInt64) -> [SpeakerSpan] {
        spans.compactMap { span in
            let lower = max(startMs, span.startMs)
            let upper = min(endMs, span.endMs)
            guard !span.speaker.isEmpty, upper > lower else { return nil }
            return SpeakerSpan(startMs: lower, endMs: upper, speaker: span.speaker)
        }
    }

    /// The protocol's segments: sorted by start; a span that touches or
    /// overlaps the same speaker's previous segment extends it; speakers are
    /// renamed `"S1"`, `"S2"`, … in order of first appearance.
    public static func final(_ spans: [SpeakerSpan]) -> [TimedSegment] {
        let sorted = spans.enumerated().sorted {
            ($0.element.startMs, $0.element.endMs, $0.offset) < ($1.element.startMs, $1.element.endMs, $1.offset)
        }.map(\.element)
        var labels: [String: String] = [:]
        var last: [String: Int] = [:]
        var merged: [(start: UInt64, end: UInt64, speaker: String)] = []
        for span in sorted {
            let label = labels[span.speaker] ?? {
                let fresh = "S\(labels.count + 1)"
                labels[span.speaker] = fresh
                return fresh
            }()
            if let index = last[label], merged[index].end >= span.startMs {
                merged[index].end = max(merged[index].end, span.endMs)
            } else {
                last[label] = merged.count
                merged.append((span.startMs, span.endMs, label))
            }
        }
        return merged.map { TimedSegment(startMs: $0.start, endMs: $0.end, speaker: $0.speaker) }
    }
}

/// What the model made of one window.
public struct WindowResult: Sendable {
    /// Speaker spans, in ms from the stream's first sample.
    public let spans: [SpeakerSpan]
    /// Speakers the stream's tracker holds after this window.
    public let speakerCount: Int
    public init(spans: [SpeakerSpan], speakerCount: Int) {
        self.spans = spans
        self.speakerCount = speakerCount
    }
}

/// One diarization stream. Holds at most one 10 s window of audio and the
/// speaker spans found so far, never the channel: memory does not grow with
/// the meeting beyond those lightweight spans.
public final class WindowedDiarizationSession: EngineSession, @unchecked Sendable {
    // @unchecked: confined to its stream's task.
    /// The window Muesli and FluidAudio's diarizer use: 10 s.
    public static let windowSamples = 10 * Int(Wire.sampleRate)
    /// Muesli's safety limit: past it the stream fails rather than merging
    /// unrelated voices or growing the speaker database without bound.
    public static let maxSpeakers = 128
    /// A whole channel of a meeting: 4 hours.
    public static let maxAudioMs: UInt64 = 14_400_000
    /// Diarize one window that starts `startSample` samples into the stream.
    public typealias Diarize = @Sendable (_ window: [Float], _ startSample: Int) async throws -> WindowResult

    private let model: String
    private let maxSamples: Int
    private let diarize: Diarize
    private var window: [Float] = []
    private var processed = 0
    private var spans: [SpeakerSpan] = []

    /// Audio held now, waiting for its window to fill.
    public var bufferedSamples: Int { window.count }

    public init(model: String, maxAudioMs: UInt64 = WindowedDiarizationSession.maxAudioMs,
                diarize: @escaping Diarize) {
        self.model = model
        self.maxSamples = Int(maxAudioMs * UInt64(Wire.sampleRate) / 1000)
        self.diarize = diarize
        window.reserveCapacity(Self.windowSamples)
    }

    public func append(_ samples: [Float]) async throws -> [EngineEvent] {
        guard processed + window.count + samples.count <= maxSamples else {
            throw SpeechdError("too_long", "a diarization stream holds at most \(maxSamples / Int(Wire.sampleRate) / 3600) hours")
        }
        var events: [EngineEvent] = []
        var rest = samples[...]
        while !rest.isEmpty {
            let take = min(rest.count, Self.windowSamples - window.count)
            window.append(contentsOf: rest.prefix(take))
            rest = rest.dropFirst(take)
            if window.count == Self.windowSamples {
                try await process()
                events.append(.partial(text: "", startMs: 0, endMs: milliseconds(samples: processed), utterance: nil))
            }
        }
        return events
    }

    public func finish() async throws -> [EngineEvent] {
        if !window.isEmpty { try await process() }
        return [.final(text: "", startMs: 0, endMs: milliseconds(samples: processed),
                       segments: DiarizationSegments.final(spans), model: model, utterance: nil)]
    }

    private func process() async throws {
        let start = processed
        let result = try await diarize(window, start)
        guard result.speakerCount <= Self.maxSpeakers else {
            throw SpeechdError("engine", "speaker tracking passed its limit of \(Self.maxSpeakers) speakers")
        }
        processed += window.count
        spans += DiarizationSegments.clipped(result.spans, startMs: milliseconds(samples: start),
                                             endMs: milliseconds(samples: processed))
        window.removeAll(keepingCapacity: true)
    }
}

/// One recognizer token with its time, as FluidAudio's `TokenTiming` gives it.
public struct TimedPiece: Sendable, Equatable {
    public let text: String
    public let startSeconds: Double
    public let endSeconds: Double
    public init(text: String, startSeconds: Double, endSeconds: Double) {
        self.text = text
        self.startSeconds = startSeconds
        self.endSeconds = endSeconds
    }
}

public enum WordTimings {
    /// SentencePiece pieces to words: a piece starting with a space or `▁`
    /// starts a new word, others continue the current one. A word runs from
    /// its first piece's start to its last piece's end.
    public static func words(_ pieces: [TimedPiece]) -> [TimedSegment] {
        var words: [TimedSegment] = []
        var text = ""
        var start = 0.0
        var end = 0.0
        func flush() {
            let word = text.trimmingCharacters(in: .whitespaces)
            if !word.isEmpty {
                words.append(TimedSegment(startMs: ms(start), endMs: ms(end), text: word))
            }
        }
        for piece in pieces where !piece.text.isEmpty {
            let normalized = piece.text.replacingOccurrences(of: "\u{2581}", with: " ")
            if normalized.hasPrefix(" ") || text.isEmpty {
                flush()
                text = normalized
                start = piece.startSeconds
            } else {
                text += normalized
            }
            end = piece.endSeconds
        }
        flush()
        return words
    }

    private static func ms(_ seconds: Double) -> UInt64 {
        UInt64(max(0, (seconds * 1000).rounded()))
    }
}
