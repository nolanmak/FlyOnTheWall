// The post-meeting pass's fakes (#246), identical to `fotw-speechd-fake`'s
// (crates/fotw-speechd-client/src/bin/fake/postpass.rs) and specified in
// docs/SPEECHD_PROTOCOL.md:
//
// - `fake-diarizer` runs the real WindowedDiarizationSession with a
//   model-free window diarizer that labels 100 ms blocks by loudness.
// - `fake-words` is a batch recognizer whose words are runs of constant
//   100 ms blocks, so windowed transcription is testable without a model.
//
// Both answer at once on `finish`.
import Foundation

public actor PostPassFakeEngine: SpeechEngine {
    public static let ids = ["fake-diarizer", "fake-words"]
    public nonisolated let descriptor: EngineDescriptor
    private var loaded = false

    public init(id: String) {
        descriptor = id == "fake-diarizer"
            ? EngineDescriptor(id: id, kind: .diarization, partials: true, languages: [],
                               maxAudioMs: WindowedDiarizationSession.maxAudioMs)
            : EngineDescriptor(id: id, kind: .batch, maxAudioMs: maxBatchMs)
    }

    public static func all() -> [any SpeechEngine] { ids.map { PostPassFakeEngine(id: $0) } }

    public func load() async throws -> LoadResult {
        loaded = true
        return LoadResult(model: descriptor.id, revision: "fake")
    }

    public func warmup() async throws {
        guard loaded else { throw SpeechdError.notLoaded(descriptor.id) }
    }

    public func unload() async { loaded = false }
    public func isLoaded() async -> Bool { loaded }

    public func makeSession(options: StreamOptions) async throws -> any EngineSession {
        guard loaded else { throw SpeechdError.notLoaded(descriptor.id) }
        if descriptor.id == "fake-diarizer" {
            return WindowedDiarizationSession(model: "fake-diarizer@fake") { window, start in
                FakeBlocks.diarize(window, startSample: start)
            }
        }
        return FakeWordsSession()
    }
}

enum FakeBlocks {
    /// 100 ms at 16 kHz.
    static let block = 1_600
    /// Below this peak (int16) a block is silence; from it, a quiet voice.
    static let quiet = 328
    /// From this peak a block is a loud voice.
    static let loud = 16_384

    static func int16(_ sample: Float) -> Int { Int((sample * 32_768).rounded()) }

    /// Maximal runs of whole blocks with the same key; a block with no key
    /// ends a run, and a trailing partial block is not classified. Returns
    /// (key, first block, last block), block indices counted from `firstBlock`.
    static func runs<Key: Equatable>(_ samples: ArraySlice<Float>, firstBlock: Int,
                                     classify: (ArraySlice<Float>) -> Key?) -> [(Key, Int, Int)] {
        var runs: [(Key, Int, Int)] = []
        var index = firstBlock
        var at = samples.startIndex
        while samples.endIndex - at >= block {
            if let key = classify(samples[at..<at + block]) {
                if let last = runs.last, last.0 == key, last.2 + 1 == index {
                    runs[runs.count - 1].2 = index
                } else {
                    runs.append((key, index, index))
                }
            }
            index += 1
            at += block
        }
        return runs
    }

    /// `fake-diarizer`'s model: one window's blocks labelled "low" or "high".
    static func diarize(_ window: [Float], startSample: Int) -> WindowResult {
        let runs = runs(window[...], firstBlock: startSample / block) { block -> String? in
            let peak = int16(max(block.max() ?? 0, -(block.min() ?? 0)))
            return peak >= loud ? "high" : peak >= quiet ? "low" : nil
        }
        let spans = runs.map { SpeakerSpan(startMs: UInt64($0.1) * 100, endMs: UInt64($0.2 + 1) * 100, speaker: $0.0) }
        return WindowResult(spans: spans, speakerCount: Set(runs.map(\.0)).count)
    }
}

/// `fake-words`: a block whose samples all equal one nonzero value v is the
/// word `w<v>`, and a run of such blocks with the same v is one word.
final class FakeWordsSession: EngineSession, @unchecked Sendable {
    // @unchecked: confined to its stream's task.
    private var pending: [Float] = []
    private var blocks = 0
    private var samples = 0
    private var runs: [(Int, Int, Int)] = []

    func append(_ chunk: [Float]) async throws -> [EngineEvent] {
        samples += chunk.count
        if samples > maxBatchSamples {
            throw SpeechdError("too_long", "a batch stream holds at most 10 minutes")
        }
        pending += chunk
        let whole = pending.count / FakeBlocks.block * FakeBlocks.block
        let found = FakeBlocks.runs(pending[..<whole], firstBlock: blocks) { block -> Int? in
            let first = FakeBlocks.int16(block[block.startIndex])
            return first != 0 && block.allSatisfy { FakeBlocks.int16($0) == first } ? first : nil
        }
        for run in found {
            if let last = runs.last, last.0 == run.0, last.2 + 1 == run.1 {
                runs[runs.count - 1].2 = run.2
            } else {
                runs.append(run)
            }
        }
        blocks += whole / FakeBlocks.block
        pending.removeFirst(whole)
        return []
    }

    func finish() async throws -> [EngineEvent] {
        let words = runs.map { TimedSegment(startMs: UInt64($0.1) * 100, endMs: UInt64($0.2 + 1) * 100, text: "w\($0.0)") }
        return [.final(text: words.compactMap(\.text).joined(separator: " "), startMs: 0,
                       endMs: milliseconds(samples: samples), segments: words, model: "fake-words@fake",
                       utterance: nil)]
    }
}
