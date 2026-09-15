import SwiftUI
import OttoFlagsSDK

@main
struct OttoFlagsExampleApp: App {
    @StateObject private var featureFlags = FeatureFlagManager()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(featureFlags)
                .task {
                    await featureFlags.initialize()
                }
        }
    }
}

@MainActor
class FeatureFlagManager: ObservableObject {
    @Published var flags: [String: Bool] = [:]
    @Published var isLoading = true

    private var client: OttoFlagsClient?

    func initialize() async {
        let config = OttoFlagsConfig(
            apiUrl: "https://beta.otto-flags.dev",
            sdkKey: "demo-sdk-key",
            environment: "development"
        )

        client = OttoFlagsClient(config: config)

        await loadFlags()
        isLoading = false
    }

    func loadFlags() async {
        let context = UserContext(
            userId: "demo-user",
            attributes: [
                "email": "demo@example.com",
                "plan": "pro"
            ]
        )

        do {
            // Load multiple feature flags
            flags["new-ui"] = try await client?.isEnabled(
                flagKey: "new-ui",
                context: context
            ) ?? false

            flags["dark-mode"] = try await client?.isEnabled(
                flagKey: "dark-mode",
                context: context
            ) ?? false

            flags["premium-features"] = try await client?.isEnabled(
                flagKey: "premium-features",
                context: context
            ) ?? false
        } catch {
            print("Error loading flags: \(error)")
        }
    }

    func isEnabled(_ key: String) -> Bool {
        return flags[key] ?? false
    }
}
