// Silero VAD through FluidAudio's streaming VadManager API. The single-flight
// handling of one stream's state follows Muesli (https://github.com/Muesli-HQ/
// muesli, MIT, © 2026 Pranav Hari), native/MuesliNative/Sources/
// MuesliNativeApp/StreamingVadController.swift at commit
// 97e455794112126a896cae17f5c829e8ffa820c8: chunks are processed strictly in
// order against one VadStreamState, here because each stream's task owns its
// session. The 0.5 speech threshold is Muesli's gate (TranscriptionRuntime.swift
// ~L1206).
//
// Changed for the helper: the Core ML model is loaded from the pinned,
// validated directory with MLModel directly, so FluidAudio's ModelHub (which
// downloads missing files) is never reached.
@preconcurrency import CoreML
import FluidAudio
import Foundation
import SpeechdCore

public actor SileroVadEngine: SpeechEngine {
    public nonisolated let descriptor: EngineDescriptor
    static let threshold: Float = 0.5
    private let store: ModelStore
    private let model: PinnedModel
    private let lifecycle = ModelLifecycle<VadManager>()

    public init(store: ModelStore, model: PinnedModel) {
        self.store = store
        self.model = model
        descriptor = EngineDescriptor(id: "silero-vad", kind: .vad, languages: [], model: model)
    }

    public func load() async throws -> LoadResult {
        let directory = try store.loadableDirectory(model)
        let config = VadConfig(defaultThreshold: Self.threshold)
        _ = try await lifecycle.load({
            let configuration = MLModelConfiguration()
            configuration.computeUnits = config.computeUnits
            let url = directory.appendingPathComponent(ModelNames.VAD.sileroVadFile)
            let coreML = try await MLModel.load(contentsOf: url, configuration: configuration)
            return VadManager(config: config, vadModel: coreML)
        }, discard: { _ in })
        return LoadResult(model: model.id, revision: model.revision)
    }

    public func warmup() async throws {
        guard let manager = await lifecycle.current else { throw SpeechdError.notLoaded(descriptor.id) }
        let state = await manager.makeStreamState()
        _ = try await manager.processStreamingChunk([Float](repeating: 0, count: VadManager.chunkSize), state: state)
    }

    public func unload() async {
        await lifecycle.shutdown()
    }

    public func isLoaded() async -> Bool { await lifecycle.current != nil }

    public func makeSession(options: StreamOptions) async throws -> any EngineSession {
        guard let manager = await lifecycle.current else { throw SpeechdError.notLoaded(descriptor.id) }
        return VadSession(manager: manager, state: await manager.makeStreamState(),
                          probabilities: options.probabilities ?? false)
    }
}

/// One stream's VAD state, fed in 256 ms chunks.
final class VadSession: EngineSession, @unchecked Sendable {
    // @unchecked: confined to its stream's task.
    private let manager: VadManager
    private let probabilities: Bool
    private var state: VadStreamState
    private var pending: [Float] = []
    private var processed = 0

    init(manager: VadManager, state: VadStreamState, probabilities: Bool) {
        self.manager = manager
        self.state = state
        self.probabilities = probabilities
    }

    func append(_ chunk: [Float]) async throws -> [EngineEvent] {
        pending.append(contentsOf: chunk)
        var events: [EngineEvent] = []
        while pending.count >= VadManager.chunkSize {
            let next = Array(pending.prefix(VadManager.chunkSize))
            pending.removeFirst(VadManager.chunkSize)
            events += try await step(next)
        }
        return events
    }

    func finish() async throws -> [EngineEvent] {
        var events: [EngineEvent] = []
        if !pending.isEmpty {
            // FluidAudio pads a short last chunk by repeating its last sample.
            events += try await step(pending)
            pending = []
        }
        if state.triggered {
            events.append(.vad(.speechEnd, sample: UInt64(processed), probability: 0))
        }
        return events
    }

    private func step(_ chunk: [Float]) async throws -> [EngineEvent] {
        let result = try await manager.processStreamingChunk(chunk, state: state)
        state = result.state
        processed += chunk.count
        var events: [EngineEvent] = []
        if probabilities {
            events.append(.vad(.probability, sample: UInt64(processed), probability: result.probability))
        }
        if let event = result.event {
            events.append(.vad(event.kind == .speechStart ? .speechStart : .speechEnd,
                               sample: UInt64(max(0, event.sampleIndex)), probability: result.probability))
        }
        return events
    }
}
