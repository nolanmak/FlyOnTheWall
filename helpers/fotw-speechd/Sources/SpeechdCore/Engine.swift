// What the server needs from an engine, and the model-free fakes the
// ordinary test suites use (docs/SPEECHD_PROTOCOL.md, "Engines").
import Foundation

public enum EngineKind: String, Sendable {
    case batch, streaming, vad, diarization
}

public struct EngineDescriptor: Sendable {
    public let id: String
    public let kind: EngineKind
    public let partials: Bool
    public let languages: [String]
    public let sampleRates: [UInt32]
    /// Most audio one stream may carry, if the engine bounds it.
    public let maxAudioMs: UInt64?
    /// The pinned model; nil for fakes.
    public let model: PinnedModel?

    public init(id: String, kind: EngineKind, partials: Bool = false, languages: [String] = ["en"],
                sampleRates: [UInt32] = [Wire.sampleRate], maxAudioMs: UInt64? = nil,
                model: PinnedModel? = nil) {
        self.maxAudioMs = maxAudioMs
        self.id = id
        self.kind = kind
        self.partials = partials
        self.languages = languages
        self.sampleRates = sampleRates
        self.model = model
    }
}

public enum VadKind: String, Sendable {
    case speechStart = "speech_start"
    case speechEnd = "speech_end"
    case probability
}

public struct TimedSegment: Sendable, Equatable {
    public let startMs: UInt64
    public let endMs: UInt64
    public let text: String?
    public let speaker: String?
    public init(startMs: UInt64, endMs: UInt64, text: String? = nil, speaker: String? = nil) {
        self.startMs = startMs
        self.endMs = endMs
        self.text = text
        self.speaker = speaker
    }
}

public enum EngineEvent: Sendable, Equatable {
    case partial(text: String, startMs: UInt64, endMs: UInt64, utterance: UInt64?)
    case final(text: String, startMs: UInt64, endMs: UInt64, segments: [TimedSegment], model: String?,
               utterance: UInt64?)
    case vad(VadKind, sample: UInt64, probability: Float)
}

public struct LoadResult: Sendable {
    public let model: String
    public let revision: String
    public init(model: String, revision: String) {
        self.model = model
        self.revision = revision
    }
}

public protocol SpeechEngine: Sendable {
    var descriptor: EngineDescriptor { get }
    /// Load from local files only. Never downloads.
    func load() async throws -> LoadResult
    /// Run one silent chunk, so the first real request pays no compile cost.
    func warmup() async throws
    func unload() async
    func isLoaded() async -> Bool
    func makeSession(options: StreamOptions) async throws -> any EngineSession
}

/// One stream's engine state. Used only from that stream's task.
public protocol EngineSession: AnyObject, Sendable {
    func append(_ samples: [Float]) async throws -> [EngineEvent]
    func finish() async throws -> [EngineEvent]
    /// The stream is over, however it ended (finished, cancelled, failed).
    /// Pooled engines take their instance back here.
    func close() async
}

extension EngineSession {
    public func close() async {}
}

/// A batch stream holds at most 10 minutes of audio.
public let maxBatchSamples = 600 * Int(Wire.sampleRate)
public let maxBatchMs: UInt64 = 600_000

public func milliseconds(samples: Int) -> UInt64 {
    UInt64(samples) * 1000 / UInt64(Wire.sampleRate)
}

// MARK: - Fakes

public actor FakeEngine: SpeechEngine {
    public static let ids = ["fake-batch", "fake-stream", "fake-vad", "fake-crash", "fake-hang", "fake-stall",
                              "fake-exit-after-final", "fake-captions"]
    public nonisolated let descriptor: EngineDescriptor
    private var loaded = false
    /// fake-captions is pooled like parakeet-eou-320ms: two streams at once.
    private var captionPool: SessionPool<Int>?
    public static let captionPoolSize = 2

    public init(id: String) {
        let kind: EngineKind = switch id {
        case "fake-stream", "fake-captions": .streaming
        case "fake-vad": .vad
        default: .batch
        }
        descriptor = EngineDescriptor(id: id, kind: kind, partials: kind == .streaming,
                                      maxAudioMs: kind == .batch ? maxBatchMs : nil)
    }

    /// Every fake, the post-meeting pass's (#246, PostPassFakes.swift) last.
    public static func all() -> [any SpeechEngine] { ids.map { FakeEngine(id: $0) } + PostPassFakeEngine.all() }

    public func load() async throws -> LoadResult {
        if descriptor.id == "fake-captions", captionPool == nil {
            captionPool = try await SessionPool(capacity: Self.captionPoolSize, make: { 0 }, warm: { _ in })
        }
        loaded = true
        return LoadResult(model: descriptor.id, revision: "fake")
    }

    public func warmup() async throws {
        guard loaded else { throw SpeechdError.notLoaded(descriptor.id) }
    }

    public func unload() async {
        loaded = false
        captionPool = nil
    }
    public func isLoaded() async -> Bool { loaded }

    public func makeSession(options: StreamOptions) async throws -> any EngineSession {
        guard loaded else { throw SpeechdError.notLoaded(descriptor.id) }
        guard let pool = captionPool else { return FakeSession(engine: descriptor.id) }
        return FakeSession(engine: descriptor.id, slot: (pool, try await pool.checkout(within: .seconds(1))))
    }
}

final class FakeSession: EngineSession, @unchecked Sendable {
    // @unchecked: confined to its stream's task.
    private let engine: String
    private var samples = 0
    private var inSpeech = false
    // fake-captions: the utterance being spoken, where it began, and how much
    // of it there is so far.
    private var utterance: UInt64 = 1
    private var utteranceStart = 0
    private var utteranceSamples = 0

    private var slot: (SessionPool<Int>, Int)?

    init(engine: String, slot: (SessionPool<Int>, Int)? = nil) {
        self.engine = engine
        self.slot = slot
    }

    func close() async {
        guard let held = slot else { return }
        slot = nil
        await held.0.checkin(held.1)
    }

    func append(_ chunk: [Float]) async throws -> [EngineEvent] {
        let start = samples
        samples += chunk.count
        switch engine {
        case "fake-crash":
            exit(70)
        case "fake-hang":
            // Alive but deaf, like a wedged runtime: the parent must notice
            // through its heartbeat and kill us.
            kill(getpid(), SIGSTOP)
            return []
        case "fake-captions":
            // Live captions without a model: an utterance runs until an
            // all-zero frame, a partial per frame within it, a final at its end.
            if chunk.allSatisfy({ $0 == 0 }) {
                return endUtterance(at: start)
            }
            if utteranceSamples == 0 { utteranceStart = start }
            utteranceSamples += chunk.count
            return [.partial(text: "utterance=\(utterance) samples=\(utteranceSamples)",
                             startMs: milliseconds(samples: utteranceStart), endMs: milliseconds(samples: samples),
                             utterance: utterance)]
        case "fake-stream":
            return [.partial(text: "samples=\(samples)", startMs: 0, endMs: milliseconds(samples: samples),
                             utterance: 1)]
        case "fake-vad":
            if inSpeech {
                guard chunk.allSatisfy({ $0 == 0 }) else { return [] }
                inSpeech = false
                return [.vad(.speechEnd, sample: UInt64(start), probability: 0)]
            }
            guard let at = chunk.firstIndex(where: { abs($0) >= 0.5 }) else { return [] }
            inSpeech = true
            return [.vad(.speechStart, sample: UInt64(start + at), probability: 1)]
        default:
            if samples > maxBatchSamples {
                throw SpeechdError("too_long", "a batch stream holds at most 10 minutes")
            }
            return []
        }
    }

    func finish() async throws -> [EngineEvent] {
        if engine == "fake-stall" {
            // A wedged engine in a helper that still answers everything else.
            while true { try await Task.sleep(nanoseconds: 3_600_000_000_000) }
        }
        if engine == "fake-batch" {
            // Slow, like a long batch on a real engine. The server keeps
            // serving everything else meanwhile (docs/SPEECHD_PROTOCOL.md).
            try await Task.sleep(nanoseconds: 2_500_000_000)
        }
        if engine == "fake-captions" {
            return endUtterance(at: samples)
        }
        if engine == "fake-vad" {
            return inSpeech ? [.vad(.speechEnd, sample: UInt64(samples), probability: 0)] : []
        }
        return [.final(text: "samples=\(samples)", startMs: 0, endMs: milliseconds(samples: samples),
                       segments: [], model: "\(engine)@fake", utterance: engine == "fake-stream" ? 1 : nil)]
    }

    private func endUtterance(at end: Int) -> [EngineEvent] {
        guard utteranceSamples > 0 else { return [] }
        let event = EngineEvent.final(
            text: "utterance=\(utterance) samples=\(utteranceSamples)",
            startMs: milliseconds(samples: utteranceStart), endMs: milliseconds(samples: end),
            segments: [], model: "fake-captions@fake", utterance: utterance)
        utterance += 1
        utteranceSamples = 0
        return [event]
    }
}
