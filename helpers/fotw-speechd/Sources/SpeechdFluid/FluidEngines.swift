import SpeechdCore

/// The model-backed engines this build serves. Nemotron 3.5 (Murmur #100) is
/// pinned in PinnedModels.swift and joins here with that issue; the diarizer
/// (#246) is pinned and added with its own.
public enum FluidEngines {
    public static func all(store: ModelStore) -> [any SpeechEngine] {
        var engines: [any SpeechEngine] = []
        if let model = PinnedModels.named("parakeet-unified") {
            engines.append(ParakeetUnifiedEngine(store: store, model: model))
        }
        if let model = PinnedModels.named("silero-vad") {
            engines.append(SileroVadEngine(store: store, model: model))
        }
        if let model = PinnedModels.named("parakeet-eou-320ms") {
            engines.append(ParakeetEouEngine(store: store, model: model))
        }
        return engines
    }
}
