import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

/// Holds the in-flight task so a cancelled caller can cancel it.
private final class TaskBox: @unchecked Sendable {
    private let lock = NSLock()
    private var task: URLSessionDataTask?
    private var cancelled = false

    func set(_ task: URLSessionDataTask) {
        lock.lock()
        let alreadyCancelled = cancelled
        self.task = task
        lock.unlock()
        if alreadyCancelled { task.cancel() }
    }

    func cancel() {
        lock.lock()
        cancelled = true
        let task = self.task
        lock.unlock()
        task?.cancel()
    }
}

extension URLSession {
    /// `data(for:)` needs iOS 15, macOS 12, tvOS 15 and watchOS 8, and Package.swift declares
    /// iOS 13, macOS 10.15, tvOS 13 and watchOS 6 (savvagent/otto-flags#119). This wraps
    /// `dataTask` in a continuation, which is available on the declared platforms and on Linux.
    func fetchData(for request: URLRequest) async throws -> (Data, URLResponse) {
        let box = TaskBox()
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                let task = self.dataTask(with: request) { data, response, error in
                    if let error {
                        continuation.resume(throwing: error)
                    } else if let response {
                        continuation.resume(returning: (data ?? Data(), response))
                    } else {
                        continuation.resume(throwing: URLError(.badServerResponse))
                    }
                }
                box.set(task)
                task.resume()
            }
        } onCancel: {
            box.cancel()
        }
    }
}
