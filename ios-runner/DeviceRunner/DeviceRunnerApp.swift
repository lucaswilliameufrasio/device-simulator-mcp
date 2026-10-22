import SwiftUI

@main
struct DeviceRunnerApp: App {
    @State private var input = ""

    var body: some Scene {
        WindowGroup {
            VStack {
                Text("Device Simulator XCTest runner")
                TextField("Runner validation input", text: $input)
                    .textFieldStyle(.roundedBorder)
                    .accessibilityIdentifier("runner-validation-input")
            }
            .padding()
        }
    }
}
