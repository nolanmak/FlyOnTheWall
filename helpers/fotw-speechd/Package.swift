// swift-tools-version: 6.0
// Model-free build: the protocol, model manager and lifecycle, the fake
// engines, and their checks. Needs only Command Line Tools. The FluidAudio
// engines are added by Package@swift-6.2.swift when FOTW_FLUIDAUDIO_SOURCE
// names a verified local FluidAudio checkout (see README.md).
import PackageDescription

let package = Package(
    name: "FotwSpeechd",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "fotw-speechd", targets: ["fotw-speechd"]),
        .executable(name: "speechd-checks", targets: ["SpeechdChecks"]),
    ],
    targets: [
        .target(name: "SpeechdCore"),
        .executableTarget(name: "fotw-speechd", dependencies: ["SpeechdCore"]),
        .executableTarget(name: "SpeechdChecks", dependencies: ["SpeechdCore"]),
    ]
)
