// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "ParakeetEval",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "parakeet-protocol-check", targets: ["ParakeetProtocolChecks"])],
    targets: [
        .target(name: "ParakeetEvalProtocol"),
        .executableTarget(name: "ParakeetProtocolChecks", dependencies: ["ParakeetEvalProtocol"]),
    ]
)
