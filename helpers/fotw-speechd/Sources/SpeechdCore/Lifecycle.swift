// Ported from Muesli (https://github.com/Muesli-HQ/muesli, MIT, © 2026 Pranav
// Hari) at commit 97e455794112126a896cae17f5c829e8ffa820c8:
// - ModelLifecycle: the generation-guarded load and shutdown of
//   native/MuesliNative/Sources/MuesliNativeApp/ParakeetUnifiedBackend.swift,
//   with the load waiters of Nemotron35StreamingBackend.swift, made generic
//   over the engine's manager type.
// - InferenceGate: native/MuesliNative/Sources/MuesliNativeApp/
//   Qwen3PostProcessor.swift, used by Nemotron35StreamingBackend.swift.
import Foundation

/// One engine's loaded manager. A `shutdown()` while a load is in flight bumps
/// the generation, so the load's result is discarded when it lands instead of
/// resurrecting a manager the caller has already let go of.
public actor ModelLifecycle<Manager: Sendable> {
    private var manager: Manager?
    private var generation: UInt64 = 0
    private var isLoading = false
    private var waiters: [CheckedContinuation<Manager, Error>] = []

    public init() {}

    public var current: Manager? { manager }

    /// Load once. Callers that arrive during a load wait for it rather than
    /// starting another.
    public func load(
        _ make: @Sendable () async throws -> Manager,
        discard: @Sendable (Manager) async -> Void
    ) async throws -> Manager {
        if let manager { return manager }
        if isLoading {
            return try await withCheckedThrowingContinuation { waiters.append($0) }
        }
        isLoading = true
        let started = generation
        let fresh: Manager
        do {
            fresh = try await make()
        } catch {
            if started == generation {
                isLoading = false
                finishWaiters(.failure(error))
            }
            throw error
        }
        // A shutdown() during the load must invalidate the result: discard the
        // freshly loaded manager instead of resurrecting a stale one.
        guard started == generation else {
            await discard(fresh)
            throw CancellationError()
        }
        manager = fresh
        isLoading = false
        finishWaiters(.success(fresh))
        return fresh
    }

    /// Forget the manager and invalidate any load in flight. Returns the
    /// manager that was loaded, for the caller to release.
    @discardableResult
    public func shutdown() -> Manager? {
        generation &+= 1
        let old = manager
        manager = nil
        isLoading = false
        finishWaiters(.failure(CancellationError()))
        return old
    }

    private func finishWaiters(_ result: Result<Manager, Error>) {
        let pending = waiters
        waiters.removeAll()
        for waiter in pending {
            waiter.resume(with: result)
        }
    }
}

/// Single-flight inference over a shared model. Actor methods re-enter while
/// Core ML predictions await, so callers serialize here while each keeps its
/// own stream state.
public actor InferenceGate {
    private struct Waiter {
        let id: UUID
        let continuation: CheckedContinuation<Bool, Never>
    }

    private var isProcessing = false
    private var waiters: [Waiter] = []

    public init() {}

    public func acquire() async throws {
        try Task.checkCancellation()
        if !isProcessing {
            isProcessing = true
            return
        }
        let id = UUID()
        let acquired = await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                waiters.append(Waiter(id: id, continuation: continuation))
            }
        } onCancel: {
            Task { await self.cancelWaiter(id) }
        }
        guard acquired else { throw CancellationError() }
    }

    public func release() {
        if waiters.isEmpty {
            isProcessing = false
            return
        }
        waiters.removeFirst().continuation.resume(returning: true)
    }

    /// Run `body` holding the gate.
    public func run<T: Sendable>(_ body: @Sendable () async throws -> T) async throws -> T {
        try await acquire()
        do {
            let value = try await body()
            release()
            return value
        } catch {
            release()
            throw error
        }
    }

    private func cancelWaiter(_ id: UUID) {
        guard let index = waiters.firstIndex(where: { $0.id == id }) else { return }
        waiters.remove(at: index).continuation.resume(returning: false)
    }
}
