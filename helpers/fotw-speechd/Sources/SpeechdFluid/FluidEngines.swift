import SpeechdCore

/// The model-backed engines this build serves. The diarizer (#246) is
/// pinned in PinnedModels.swift and joins here with its issue.
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
            engines.append(ParakeetEouEngine(store: store, model: model, chunkSize: .ms320))
        }
        if let model = PinnedModels.named("parakeet-eou-160ms") {
            engines.append(ParakeetEouEngine(store: store, model: model, chunkSize: .ms160))
        }
        if let model = PinnedModels.named("nemotron-3.5") {
            engines.append(NemotronStreamingEngine(store: store, model: model))
        }
        return engines
    }
}
