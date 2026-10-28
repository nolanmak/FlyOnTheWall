// Ported from Muesli (https://github.com/Muesli-HQ/muesli, MIT, © 2026 Pranav
// Hari), native/MuesliNative/Sources/MuesliNativeApp/ParakeetUnifiedBackend.swift
// at commit 97e455794112126a896cae17f5c829e8ffa820c8: FluidAudio's offline
// `UnifiedAsrManager` with the int8 encoder, loaded behind a generation guard
// (SpeechdCore.ModelLifecycle). Warmup follows TranscriptionRuntime.swift
// (~L700-730): one silent chunk at load so the first request pays no Core ML
// compile.
//
// Changed for the helper: models load only from the pinned, validated
// directory (never downloaded here), audio arrives as PCM over the protocol
// instead of a WAV file, and inference is single-flight per engine.
import FluidAudio
import Foundation
import SpeechdCore

public actor ParakeetUnifiedEngine: SpeechEngine {
    public nonisolated let descriptor: EngineDescriptor
    private let store: ModelStore
    private let model: PinnedModel
    private let lifecycle = ModelLifecycle<UnifiedAsrManager>()
    private let gate = InferenceGate()

    public init(store: ModelStore, model: PinnedModel) {
        self.store = store
        self.model = model
        descriptor = EngineDescriptor(id: "parakeet-unified", kind: .batch, maxAudioMs: maxBatchMs, model: model)
    }

    public func load() async throws -> LoadResult {
        let directory = try store.loadableDirectory(model)
        _ = try await lifecycle.load({
            let manager = UnifiedAsrManager(encoderPrecision: .int8)
            // The explicit-directory overload reads local files only.
            try await manager.loadModels(from: directory)
            return manager
        }, discard: { [gate] stale in
            // Nothing can be using a manager that was never published, but
            // take the gate anyway: cleanup and inference never overlap.
            try? await gate.run { await stale.cleanup() }
        })
        return LoadResult(model: model.id, revision: model.revision)
    }

    public func warmup() async throws {
        _ = try await transcribe([Float](repeating: 0, count: Int(Wire.sampleRate)))
    }

    public func unload() async {
        // A transcription may still hold the manager. Release it only once
        // that finishes: cleanup runs under the same gate as inference.
        if let old = await lifecycle.shutdown() {
            try? await gate.run { await old.cleanup() }
        }
    }

    public func isLoaded() async -> Bool { await lifecycle.current != nil }

    public func makeSession(options: StreamOptions) async throws -> any EngineSession {
        guard await lifecycle.current != nil else { throw SpeechdError.notLoaded(descriptor.id) }
        return BatchSession(engine: self, provenance: "\(model.id)@\(model.revision)")
    }

    func transcribe(_ samples: [Float]) async throws -> String {
        guard let manager = await lifecycle.current else { throw SpeechdError.notLoaded(descriptor.id) }
        return try await gate.run { try await manager.transcribe(samples) }
    }
}

/// Buffers a stream's audio and transcribes it on finish.
final class BatchSession: EngineSession, @unchecked Sendable {
    // @unchecked: confined to its stream's task.
    private let engine: ParakeetUnifiedEngine
    private let provenance: String
    private var samples: [Float] = []

    init(engine: ParakeetUnifiedEngine, provenance: String) {
        self.engine = engine
        self.provenance = provenance
    }

    func append(_ chunk: [Float]) async throws -> [EngineEvent] {
        guard samples.count + chunk.count <= maxBatchSamples else {
            throw SpeechdError("too_long", "a batch stream holds at most 10 minutes")
        }
        samples.append(contentsOf: chunk)
        return []
    }

    func finish() async throws -> [EngineEvent] {
        let endMs = milliseconds(samples: samples.count)
        let text = samples.isEmpty ? "" : try await engine.transcribe(samples)
        samples = []
        return [.final(text: text.trimmingCharacters(in: .whitespacesAndNewlines), startMs: 0, endMs: endMs,
                       segments: [], model: provenance, utterance: nil)]
    }
}
