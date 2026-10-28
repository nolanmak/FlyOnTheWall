// Every model fotw-speechd can install, pinned to a full revision with the
// size and SHA-256 of each file. Read from the Hugging Face API
// (`/api/models/<repo>/tree/<revision>?recursive=true`: `lfs.oid` is the
// SHA-256 of an LFS file; small non-LFS files were downloaded and hashed, and
// their git blob SHA-1 checked against the API's `oid`).
//
// The selection follows Muesli's model plans (ManagedASRModelDownloads.swift,
// Nemotron35ModelStore.swift at 97e455794112126a896cae17f5c829e8ffa820c8):
// - parakeet-unified: the int8 encoder, decoder, single-step joint, vocab and
//   metadata (README.md left out).
// - silero-vad: the unified 256 ms v6.2.1 model FluidAudio 0.17.3 loads.
// - parakeet-eou-320ms: the 320 ms streaming encoder, decoder, joint and vocab.
// - nemotron-3.5-2240ms: multilingual/2240ms without decoder_joint.mlmodelc,
//   which Muesli skips as well.
// Weights are downloaded on demand by an explicit install and never committed.

public struct PinnedFile: Sendable, Equatable {
    public let path: String
    public let size: Int64
    public let sha256: String
    public init(path: String, size: Int64, sha256: String) {
        self.path = path
        self.size = size
        self.sha256 = sha256
    }
}

public struct PinnedModel: Sendable, Equatable {
    public let id: String
    public let repository: String
    public let revision: String
    /// Shown in the model UI and the notices.
    public let license: String
    /// The files' directory inside the repository, "" for its root.
    public let remoteDirectory: String
    public let files: [PinnedFile]
    public init(id: String, repository: String, revision: String, license: String,
                remoteDirectory: String, files: [PinnedFile]) {
        self.id = id
        self.repository = repository
        self.revision = revision
        self.license = license
        self.remoteDirectory = remoteDirectory
        self.files = files
    }

    public var totalBytes: Int64 { files.reduce(0) { $0 + $1.size } }
}

public enum PinnedModels {
    public static func named(_ id: String) -> PinnedModel? { all.first { $0.id == id } }

    public static let all: [PinnedModel] = [
        PinnedModel(id: "parakeet-unified", repository: "FluidInference/parakeet-unified-en-0.6b-coreml", revision: "d32e972dd4315f1dc3f6be28fb2aab0ab3e80358", license: "CC-BY-4.0", remoteDirectory: "", files: [
            PinnedFile(path: "metadata.json", size: 1046, sha256: "2b26a96b76fe1f7a04d3e867f50c75d6ce5dd1650d0dbcd4c35b591b22305f0e"),
            PinnedFile(path: "parakeet_unified_decoder.mlmodelc/analytics/coremldata.bin", size: 243, sha256: "9ae70f6559989f88b856b326e59315798f9f0d08207a19fcc2dd3287a30088a5"),
            PinnedFile(path: "parakeet_unified_decoder.mlmodelc/coremldata.bin", size: 560, sha256: "ce99c4488840fc463d59f8d4d6d2a9e8ceae8138ead51e3c265dde4d2ba4a0e9"),
            PinnedFile(path: "parakeet_unified_decoder.mlmodelc/model.mil", size: 13102, sha256: "6e60965b89c93943aa2be2d991c2461108145851fde05e1d048223a32d4cb20d"),
            PinnedFile(path: "parakeet_unified_decoder.mlmodelc/weights/weight.bin", size: 14429952, sha256: "96f990461a5986d5e7309ad1a0f36084fbf0f4b28aec35948f8b8d0dcbf8599e"),
            PinnedFile(path: "parakeet_unified_encoder_int8.mlmodelc/analytics/coremldata.bin", size: 243, sha256: "57e116a9d5765e39c0cdf754137ab744ddae34d9c6d68a5fdcad6600ae3a7b6b"),
            PinnedFile(path: "parakeet_unified_encoder_int8.mlmodelc/coremldata.bin", size: 492, sha256: "54f533d30343d5e62b324a0691e4c262a6768b07b6e88e7aa14c617a2baba8a3"),
            PinnedFile(path: "parakeet_unified_encoder_int8.mlmodelc/model.mil", size: 1110902, sha256: "c1c5d71c6cbf4d35bba08458746bde3640da7b1b444e1229a269393a58222c10"),
            PinnedFile(path: "parakeet_unified_encoder_int8.mlmodelc/weights/weight.bin", size: 595051904, sha256: "f984b81590a4deae041ae20fbab8981c2d2a5b528b2ac81fae81c432633535c6"),
            PinnedFile(path: "parakeet_unified_joint_decision_single_step.mlmodelc/analytics/coremldata.bin", size: 243, sha256: "163877ad14af97ec4107cd854fd1c6d336ee5d40ad25a657cc764fb763f452f5"),
            PinnedFile(path: "parakeet_unified_joint_decision_single_step.mlmodelc/coremldata.bin", size: 556, sha256: "68a081570a48b52ec9379e153bd56748a5408a50be16767601563f231eaeff03"),
            PinnedFile(path: "parakeet_unified_joint_decision_single_step.mlmodelc/model.mil", size: 9611, sha256: "03c21096090bcd0b71c896c5ae0eb815db31a91c6676f572a7868eee4299abe3"),
            PinnedFile(path: "parakeet_unified_joint_decision_single_step.mlmodelc/weights/weight.bin", size: 3446978, sha256: "06831afa6d1beb0c0b10350ebf7886bc37638e951d14e738d7e06fbd2a05012f"),
            PinnedFile(path: "vocab.json", size: 15088, sha256: "e1a7bff4f5df133c0f4ad47b8e43c96f6bf1865d99126a4c4725ef51d0108bec"),
        ]),
        PinnedModel(id: "silero-vad", repository: "FluidInference/silero-vad-coreml", revision: "b419383c55c110e2c9271fa6ee0ea83d03c70d96", license: "MIT", remoteDirectory: "", files: [
            PinnedFile(path: "silero-vad-unified-256ms-v6.2.1.mlmodelc/analytics/coremldata.bin", size: 243, sha256: "8067594eb3126ab8318af507f0c00cabfed40d5fedb8a0ee5075dd02e903d909"),
            PinnedFile(path: "silero-vad-unified-256ms-v6.2.1.mlmodelc/coremldata.bin", size: 625, sha256: "7db35a4fd995222a7fb0129713473b15d1462572ab4a2e5e4d56bcaad9e40f41"),
            PinnedFile(path: "silero-vad-unified-256ms-v6.2.1.mlmodelc/metadata.json", size: 3335, sha256: "2740be542c611e1ba358e1849b4e265c65cdf0b17192767e1e5de86a31ac94d6"),
            PinnedFile(path: "silero-vad-unified-256ms-v6.2.1.mlmodelc/model.mil", size: 176918, sha256: "c6a9d1bf22d413265da0a07a1d14151c3ea2fad296b3aa5859275b33ef1c3270"),
            PinnedFile(path: "silero-vad-unified-256ms-v6.2.1.mlmodelc/weights/weight.bin", size: 882304, sha256: "53ecc8b5081146140ab654c89109cf001f2183abddd7a2411c5081feeffff063"),
        ]),
        PinnedModel(id: "parakeet-eou-320ms", repository: "FluidInference/parakeet-realtime-eou-120m-coreml", revision: "40a23f4c0b333aa17ad8c0f2ea47ec2347f2f355", license: "NVIDIA Open Model License", remoteDirectory: "320ms", files: [
            PinnedFile(path: "decoder.mlmodelc/analytics/coremldata.bin", size: 243, sha256: "3996975a8cbc1949159c55605b3132b39b2484f51acbd55d796d93c70de02b49"),
            PinnedFile(path: "decoder.mlmodelc/coremldata.bin", size: 497, sha256: "c3ccbff963d8cf07e2be2bd56ea3384a89ea49628922c6bd95ff62e2ae57dc34"),
            PinnedFile(path: "decoder.mlmodelc/metadata.json", size: 3283, sha256: "0977480649f2756894b0acfe2fdf4231a991f25e3fe02562bfb71b65ca944575"),
            PinnedFile(path: "decoder.mlmodelc/model.mil", size: 7409, sha256: "b7c084a35bdbc887d69d6226cd533e2c11b2792c37d7352cf878f9f6f3c13555"),
            PinnedFile(path: "decoder.mlmodelc/weights/weight.bin", size: 7873600, sha256: "0b4cacecdcd9df79ab1e56de67230baf5a8664d2afe0bb8f3408eefa972cb2f4"),
            PinnedFile(path: "joint_decision.mlmodelc/analytics/coremldata.bin", size: 243, sha256: "5bca32ad130dcad6605cc00044c752aa5b45ef57d14c17f2d1a2fa49d6cf55b5"),
            PinnedFile(path: "joint_decision.mlmodelc/coremldata.bin", size: 493, sha256: "22d4abc4625b935ee035b5f8ce7cb28d1041b9b01c12173e287bf4b5f5d99625"),
            PinnedFile(path: "joint_decision.mlmodelc/metadata.json", size: 3181, sha256: "e970ae87137730020690d24d971813db3633bbdfed602d43b6a9c84deced6dc8"),
            PinnedFile(path: "joint_decision.mlmodelc/model.mil", size: 9608, sha256: "45e8590bc87e34c162b547e43a4f60e64db15b017f48395d7835a6867884804f"),
            PinnedFile(path: "joint_decision.mlmodelc/weights/weight.bin", size: 2794182, sha256: "7039b2010a269153f5a96edf28637f921a86ef8822f248f2d6712f7a6bce84b4"),
            PinnedFile(path: "streaming_encoder.mlmodelc/analytics/coremldata.bin", size: 243, sha256: "ae0a559f8d53c43fe13c5c142b15ca237b97812febde71a36475b7b9612baec5"),
            PinnedFile(path: "streaming_encoder.mlmodelc/coremldata.bin", size: 670, sha256: "b57dfe32ed2acb0081f97d7a9e410117005823e3d2d995e21feb9e60775443c3"),
            PinnedFile(path: "streaming_encoder.mlmodelc/metadata.json", size: 5327, sha256: "3adc7839db0f5d20b19e2dcd1c009bcf935a7edc61d7473c8150be444bd80488"),
            PinnedFile(path: "streaming_encoder.mlmodelc/model.mil", size: 655998, sha256: "eb9d56d7467ad3a8796ec9098b27fb89197638b31e24ede6dd924807d25086e6"),
            PinnedFile(path: "streaming_encoder.mlmodelc/weights/weight.bin", size: 212865856, sha256: "ff553fcb4f4560cfce1e558e16bb9d856e4b4f6a4c08f800d5c126fce4196822"),
            PinnedFile(path: "vocab.json", size: 17437, sha256: "83fd42ad33dae1bd3ceee6c0bb6c625f314cf0b2dc8430be441ac1e2643d5c36"),
        ]),
        PinnedModel(id: "nemotron-3.5-2240ms", repository: "FluidInference/Nemotron-3.5-ASR-Streaming-Multilingual-0.6b-CoreML", revision: "1a41b75758b0337ff67db7d5408280aaaf23074e", license: "OpenMDW-1.1", remoteDirectory: "multilingual/2240ms", files: [
            PinnedFile(path: "decoder.mlmodelc/analytics/coremldata.bin", size: 243, sha256: "fdb14a08e42b4806a2d1505501586be71e4f04ca9256c719544fd7ed6937e509"),
            PinnedFile(path: "decoder.mlmodelc/coremldata.bin", size: 433, sha256: "3a89047b6f74ee3d0a74c72f8c8e5016d76d9487d030f0661fe80220b441a6fd"),
            PinnedFile(path: "decoder.mlmodelc/model.mil", size: 11743, sha256: "f3ed3e9cac9b70e1b00df48b53868dbd3c4dd8cf158af79ded0d05a96e0bf5dc"),
            PinnedFile(path: "decoder.mlmodelc/weights/weight.bin", size: 29870592, sha256: "dcdeccd4ccf46e2675224f9f030d46c1a89e2bda4abb316e901e1a21f1597f8f"),
            PinnedFile(path: "encoder.mlmodelc/analytics/coremldata.bin", size: 243, sha256: "af569fd95237bdf8b91d38094691bfc990fcf3534316fa615bac5964c02809e4"),
            PinnedFile(path: "encoder.mlmodelc/coremldata.bin", size: 573, sha256: "d5471a4edf55ce02dab51e5d01c86b419a7659bcdd6aac216ba6f0b795eae8bf"),
            PinnedFile(path: "encoder.mlmodelc/model.mil", size: 1010788, sha256: "3ee65463908bbc06c92691ea9172499aea3f7ae4ba619d39783c4a639936fae5"),
            PinnedFile(path: "encoder.mlmodelc/weights/weight.bin", size: 565336640, sha256: "2e00be98049a22e095452c020f183d2b23728e145cc814ba031436931b4f2e8f"),
            PinnedFile(path: "joint.mlmodelc/analytics/coremldata.bin", size: 243, sha256: "a1a90a7d5f8b86f564a42ab45b42a43eb0bbce6682176916bd94591b28cca447"),
            PinnedFile(path: "joint.mlmodelc/coremldata.bin", size: 341, sha256: "8f750980da8ea3397d860f69e0755d3adf61ee763d7f189405d50d4e2d9f8ca0"),
            PinnedFile(path: "joint.mlmodelc/model.mil", size: 5072, sha256: "521da827005ddcf3fbd439d12f85c34b1273642635b39a87bfbc931131cd3bf6"),
            PinnedFile(path: "joint.mlmodelc/weights/weight.bin", size: 18911744, sha256: "c0ef0a3a6598f962d2aad598dc6850e4428874033419817121e11f1fff4a9cfe"),
            PinnedFile(path: "metadata.json", size: 3005, sha256: "070ae181941003ff3e7d7ff8e5c5d47aebd026e8458549cef0c9a6803dbec004"),
            PinnedFile(path: "preprocessor.mlmodelc/analytics/coremldata.bin", size: 243, sha256: "e918fd75105ef01a971d29b5ec28f531467b42dd60978c29afb1914c4af838af"),
            PinnedFile(path: "preprocessor.mlmodelc/coremldata.bin", size: 371, sha256: "3d1aa8c8e7e283e4944af4b0b701db760ed99ef14919d3f989c599b9f63335a2"),
            PinnedFile(path: "preprocessor.mlmodelc/model.mil", size: 18449, sha256: "12637d7ddfabea2d58e6b7986699c1be7fe970dc589eeac52fcb9ad25ae06ec9"),
            PinnedFile(path: "preprocessor.mlmodelc/weights/weight.bin", size: 592384, sha256: "297514e2b211d14b0e53cb97193d679bb89ead98d28e578f3f1d049ddbcc36b3"),
            PinnedFile(path: "tokenizer.json", size: 284969, sha256: "fb70c8fcb6472cda2bdb799b156a8941e72762344a7c636d3b1275f1d53c4a6b"),
        ]),
    ]
}
