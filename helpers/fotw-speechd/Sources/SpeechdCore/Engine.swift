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
                              "fake-exit-after-final"]
    public nonisolated let descriptor: EngineDescriptor
    private var loaded = false

    public init(id: String) {
        let kind: EngineKind = switch id {
        case "fake-stream": .streaming
        case "fake-vad": .vad
        default: .batch
        }
        descriptor = EngineDescriptor(id: id, kind: kind, partials: id == "fake-stream",
                                      maxAudioMs: kind == .batch ? maxBatchMs : nil)
    }

    /// Every fake, the post-meeting pass's (#246, PostPassFakes.swift) last.
    public static func all() -> [any SpeechEngine] { ids.map { FakeEngine(id: $0) } + PostPassFakeEngine.all() }

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
        return FakeSession(engine: descriptor.id)
    }
}

final class FakeSession: EngineSession, @unchecked Sendable {
    // @unchecked: confined to its stream's task.
    private let engine: String
    private var samples = 0
    private var inSpeech = false

    init(engine: String) {
        self.engine = engine
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
        if engine == "fake-vad" {
            return inSpeech ? [.vad(.speechEnd, sample: UInt64(samples), probability: 0)] : []
        }
        return [.final(text: "samples=\(samples)", startMs: 0, endMs: milliseconds(samples: samples),
                       segments: [], model: "\(engine)@fake", utterance: engine == "fake-stream" ? 1 : nil)]
    }
}
