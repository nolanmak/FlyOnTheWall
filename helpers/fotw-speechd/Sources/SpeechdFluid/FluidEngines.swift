import SpeechdCore

/// The model-backed engines this build serves. Parakeet realtime EOU (#245)
/// and Nemotron 3.5 (Murmur #100) are pinned in PinnedModels.swift and join
/// here as those issues land.
public enum FluidEngines {
    public static func all(store: ModelStore) -> [any SpeechEngine] {
        var engines: [any SpeechEngine] = []
        if let model = PinnedModels.named("parakeet-unified") {
            engines.append(ParakeetUnifiedEngine(store: store, model: model))
        }
        if let model = PinnedModels.named("silero-vad") {
            engines.append(SileroVadEngine(store: store, model: model))
        }
        if let model = PinnedModels.named("diarizer") {
            engines.append(DiarizerEngine(store: store, model: model))
        }
        return engines
    }
}
