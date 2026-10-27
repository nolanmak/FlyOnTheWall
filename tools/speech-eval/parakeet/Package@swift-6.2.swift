// swift-tools-version: 6.2
import Foundation
import PackageDescription

var dependencies: [Package.Dependency] = []
var products: [Product] = [.executable(name: "parakeet-protocol-check", targets: ["ParakeetProtocolChecks"])]
var targets: [Target] = [
    .target(name: "ParakeetEvalProtocol"),
    .executableTarget(name: "ParakeetProtocolChecks", dependencies: ["ParakeetEvalProtocol"]),
]
if let source = ProcessInfo.processInfo.environment["FOTW_FLUIDAUDIO_SOURCE"] {
    // Explicit local dependency only. This manifest never clones a repository
    // or resolves a model. Disable linking the unrelated text-normalizer.
    // Upstream SwiftPM may still resolve its declared binary artifact at build time.
    dependencies.append(.package(path: source, traits: []))
    products.append(.executable(name: "parakeet-eval", targets: ["ParakeetEval"]))
    targets.append(.executableTarget(name: "ParakeetEval", dependencies: [
        "ParakeetEvalProtocol",
        .product(name: "FluidAudio", package: URL(fileURLWithPath: source).lastPathComponent.lowercased()),
    ]))
}
let package = Package(name: "ParakeetEval", platforms: [.macOS(.v14)],
                      products: products, dependencies: dependencies, targets: targets)
