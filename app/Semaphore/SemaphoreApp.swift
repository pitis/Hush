import SwiftUI

@main
struct SemaphoreApp: App {
    /// XCTest launches the app as its test host; it must not touch real data or the network then.
    private let isTestHost = ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil

    var body: some Scene {
        WindowGroup {
            if isTestHost { EmptyView() } else { ContentView() }
        }
    }
}
