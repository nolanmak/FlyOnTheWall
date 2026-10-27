import Foundation
import FluidAudio
import ParakeetEvalProtocol

@main
struct ParakeetEvaluation {
    static func main() async {
        do {
            let request = try Invocation.parse(Array(CommandLine.arguments.dropFirst()))
            try request.validateModelDirectory()
            let input = try AudioInput.load(request.audio)
            // The explicit-directory overload only loads local files. Never call
            // downloadAndLoad/loadModels(to:) or any default cache discovery.
            let engine = UnifiedAsrManager(encoderPrecision: .int8)
            try await engine.loadModels(from: request.models)
            let text = try await engine.transcribe(input.samples)
            await engine.cleanup()
            let encoded = try JSONEncoder().encode(RawResponse(audioSHA256: input.sha256, text: text))
            guard encoded.count <= 1_048_576 else { throw EvaluationError.audio }
            try FileHandle.standardOutput.write(contentsOf: encoded + Data([10]))
        } catch {
            // No input paths, model internals or transcript in adapter errors.
            try? FileHandle.standardError.write(contentsOf: Data("parakeet evaluation failed\n".utf8))
            exit(2)
        }
    }
}
