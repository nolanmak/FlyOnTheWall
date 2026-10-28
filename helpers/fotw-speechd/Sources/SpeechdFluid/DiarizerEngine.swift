// Ported from Muesli (https://github.com/Muesli-HQ/muesli, MIT, © 2026 Pranav
// Hari) at commit 97e455794112126a896cae17f5c829e8ffa820c8:
// - native/MuesliNative/Sources/MuesliNativeApp/MeetingRecordingTranscriber.swift,
//   `RecordedAudioDiarizationSession`: each job swaps its own reset
//   SpeakerManager into the shared DiarizerManager around a window's
//   `performCompleteDiarization(samples, sampleRate: 16_000, atTime:)`, so
//   speaker identities never leak between jobs, and checks the 128-speaker
//   cap after it. The window and the cap are in SpeechdCore/Diarization.swift.
// - native/MuesliNative/Sources/MuesliNativeApp/TranscriptionRuntime.swift
//   (~L900-920): `DiarizerManager()` + `initialize(models:)` on loaded
//   `DiarizerModels`; `diarizeRecordedAudio` (~L1234) runs the windows.
//
// Changed for the helper: the models load only from the pinned, validated
// directory with `DiarizerModels.load(localSegmentationModel:localEmbeddingModel:)`,
// which reads local files and never reaches FluidAudio's ModelHub; audio
// arrives as PCM over the protocol instead of from a file; and inference is
// single-flight per engine (InferenceGate), as in ParakeetUnifiedEngine.
import FluidAudio
import Foundation
import SpeechdCore

/// FluidAudio's DiarizerManager is a non-Sendable class. Every use of it
/// happens under the engine's InferenceGate, one at a time.
final class DiarizerBox: @unchecked Sendable {
    let manager: DiarizerManager
    init(_ manager: DiarizerManager) { self.manager = manager }
}

public actor DiarizerEngine: SpeechEngine {
    public nonisolated let descriptor: EngineDescriptor
    private let store: ModelStore
    private let model: PinnedModel
    private let lifecycle = ModelLifecycle<DiarizerBox>()
    private let gate = InferenceGate()

    public init(store: ModelStore, model: PinnedModel) {
        self.store = store
        self.model = model
        descriptor = EngineDescriptor(id: "diarizer", kind: .diarization, partials: true, languages: [],
                                      maxAudioMs: WindowedDiarizationSession.maxAudioMs, model: model)
    }

    public func load() async throws -> LoadResult {
        let directory = try store.loadableDirectory(model)
        _ = try await lifecycle.load({
            let models = try DiarizerModels.load(
                localSegmentationModel: directory.appendingPathComponent(ModelNames.Diarizer.segmentationFile),
                localEmbeddingModel: directory.appendingPathComponent(ModelNames.Diarizer.embeddingFile))
            let manager = DiarizerManager()
            manager.initialize(models: models)
            return DiarizerBox(manager)
        }, discard: { [gate] stale in
            try? await gate.run { stale.manager.cleanup() }
        })
        return LoadResult(model: model.id, revision: model.revision)
    }

    /// One silent window through a throwaway speaker state.
    public func warmup() async throws {
        let speakers = try await initialSpeakers()
        _ = try await diarize([Float](repeating: 0, count: WindowedDiarizationSession.windowSamples),
                              startSample: 0, speakers: speakers)
    }

    public func unload() async {
        if let old = await lifecycle.shutdown() {
            try? await gate.run { old.manager.cleanup() }
        }
    }

    public func isLoaded() async -> Bool { await lifecycle.current != nil }

    public func makeSession(options: StreamOptions) async throws -> any EngineSession {
        let state = StreamSpeakers(try await initialSpeakers())
        return WindowedDiarizationSession(model: "\(model.id)@\(model.revision)") { [self] window, start in
            let (result, speakers) = try await self.diarize(window, startSample: start, speakers: state.speakers)
            state.speakers = speakers
            return result
        }
    }

    /// A reset copy of the manager's speaker state: the stream's own.
    private func initialSpeakers() async throws -> SpeakerManager {
        guard let box = await lifecycle.current else { throw SpeechdError.notLoaded(descriptor.id) }
        return try await gate.run {
            var speakers = box.manager.speakerManager
            speakers.reset()
            return speakers
        }
    }

    /// Diarize one window with `speakers` swapped in, as Muesli's
    /// `withSpeakerState` does, and return the window's spans and the
    /// speaker state after it.
    private func diarize(_ window: [Float], startSample: Int,
                         speakers: SpeakerManager) async throws -> (WindowResult, SpeakerManager) {
        guard let box = await lifecycle.current else { throw SpeechdError.notLoaded(descriptor.id) }
        return try await gate.run {
            try Task.checkCancellation()
            let manager = box.manager
            let previous = manager.speakerManager
            manager.speakerManager = speakers
            defer { manager.speakerManager = previous }
            let result = try autoreleasepool {
                try manager.performCompleteDiarization(
                    window, sampleRate: Int(Wire.sampleRate),
                    atTime: Double(startSample) / Double(Wire.sampleRate))
            }
            let spans = result.segments.compactMap { segment -> SpeakerSpan? in
                guard segment.startTimeSeconds.isFinite, segment.endTimeSeconds.isFinite else { return nil }
                return SpeakerSpan(startMs: Self.ms(segment.startTimeSeconds), endMs: Self.ms(segment.endTimeSeconds),
                                   speaker: segment.speakerId)
            }
            return (WindowResult(spans: spans, speakerCount: manager.speakerManager.speakerCount),
                    manager.speakerManager)
        }
    }

    private static func ms(_ seconds: Float) -> UInt64 {
        UInt64(max(0, (Double(seconds) * 1000).rounded()))
    }
}

/// One stream's speaker tracker, carried from window to window. Used only from
/// that stream's task, one window at a time.
final class StreamSpeakers: @unchecked Sendable {
    var speakers: SpeakerManager
    init(_ speakers: SpeakerManager) { self.speakers = speakers }
}
