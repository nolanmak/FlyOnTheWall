// swift-tools-version: 6.2
// Same as Package.swift, plus the FluidAudio engines when
// FOTW_FLUIDAUDIO_SOURCE names a local FluidAudio checkout that
// tools/speech-eval/parakeet/verify_runtime.py accepts (0.17.3 at
// a6b826ce9a09028fc685a4b32a8927ec920c3188). This manifest never clones a
// repository or fetches a model.
import Foundation
import PackageDescription

var dependencies: [Package.Dependency] = []
var helperDependencies: [Target.Dependency] = ["SpeechdCore"]
var helperSettings: [SwiftSetting] = []
var targets: [Target] = [
    .target(name: "SpeechdCore"),
    .executableTarget(name: "SpeechdChecks", dependencies: ["SpeechdCore"]),
]
if let source = ProcessInfo.processInfo.environment["FOTW_FLUIDAUDIO_SOURCE"] {
    // No traits: the NeMo text normalizer is not linked.
    dependencies.append(.package(path: source, traits: []))
    targets.append(.target(name: "SpeechdFluid", dependencies: [
        "SpeechdCore",
        .product(name: "FluidAudio", package: URL(fileURLWithPath: source).lastPathComponent.lowercased()),
    ]))
    helperDependencies.append("SpeechdFluid")
    helperSettings.append(.define("FOTW_FLUIDAUDIO"))
}
targets.append(.executableTarget(name: "fotw-speechd", dependencies: helperDependencies,
                                 swiftSettings: helperSettings))

let package = Package(
    name: "FotwSpeechd",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "fotw-speechd", targets: ["fotw-speechd"]),
        .executable(name: "speechd-checks", targets: ["SpeechdChecks"]),
    ],
    dependencies: dependencies,
    targets: targets
)
