import SwiftUI

@main
struct SemaphoreApp: App {
    /// XCTest launches the app as its test host; it must not touch real data or the network then.
    private let isTestHost = ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil
    /// One core per app (it locks the data folder), owned by the app so closing the window doesn't
    /// tear it down.
    @State private var core = CoreBridge()

    var body: some Scene {
        Window("Semaphore", id: "main") {
            if isTestHost { EmptyView() } else { ContentView(core: core) }
        }
    }
}
