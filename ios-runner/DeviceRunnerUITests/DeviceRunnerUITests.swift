import Foundation
import XCTest

final class DeviceRunnerUITests: XCTestCase {
    func testPersistentCommandServer() throws {
        try DeviceRunnerCommandServer(port: 39173).run()
    }

    func testFileCommandServer() throws {
        guard let mailbox = ProcessInfo.processInfo.environment["DEVICE_RUNNER_MAILBOX"],
              !mailbox.isEmpty,
              mailbox.count <= 128,
              mailbox.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "." || $0 == "-") }),
              mailbox != ".",
              mailbox != ".." else {
            throw NSError(domain: "DeviceRunner", code: 1)
        }
        try DeviceRunnerCommandServer(port: 39173).runFiles(mailbox: mailbox)
    }
}
