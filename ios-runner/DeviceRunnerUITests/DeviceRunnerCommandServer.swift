import Darwin
import Foundation
import XCTest

private let maximumRequestBytes = 1_048_576
private let maximumResponseBytes = 6_291_456
private let maximumAccessibilityElements = 200
private let maximumTextBytes = 4096
private let springboardBundleIdentifier = "com.apple.springboard"

struct DeviceRunnerCommandServer {
    let port: UInt16

    func runFiles(mailbox: String) throws {
        let documents = try FileManager.default.url(
            for: .documentDirectory, in: .userDomainMask, appropriateFor: nil, create: true
        )
        let directory = documents.appendingPathComponent(mailbox, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer {
            try? FileManager.default.removeItem(at: directory)
        }
        let ready = try JSONSerialization.data(withJSONObject: ["mailbox": mailbox])
        try ready.write(to: directory.appendingPathComponent("ready.json"), options: .atomic)
        var lastRequestNumber: UInt64 = 0
        var shouldStop = false
        while !shouldStop {
            Thread.sleep(forTimeInterval: 0.1)
            guard let commit = try readMailboxFile(
                directory.appendingPathComponent("commit.json"), maximumBytes: 512
            ),
                  let marker = (try? JSONSerialization.jsonObject(with: commit)) as? [String: Any],
                  let committedId = marker["request_id"] as? String,
                  let committedNumber = UInt64(committedId),
                  committedNumber > lastRequestNumber else {
                continue
            }
            let requestURL = directory.appendingPathComponent("request.json")
            guard let data = try readMailboxFile(requestURL, maximumBytes: maximumRequestBytes) else {
                continue
            }
            guard data.count <= maximumRequestBytes,
                  let fields = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
                  let requestId = fields["request_id"] as? String,
                  requestId == committedId,
                  let requestNumber = UInt64(requestId),
                  requestNumber > lastRequestNumber else {
                // A device copy may still be incomplete. Never execute partial JSON.
                continue
            }
            // Claim the ID before input. A repeated file must never replay an action.
            lastRequestNumber = requestNumber
            try FileManager.default.removeItem(at: requestURL)
            let response = try executeSingleRequest(data)
            guard response.count <= maximumResponseBytes else {
                throw RunnerError.responseTooLarge
            }
            try response.write(to: directory.appendingPathComponent("response.json"), options: .atomic)
            shouldStop = fields["command"] as? String == "shutdown"
        }
    }

    private func readMailboxFile(_ url: URL, maximumBytes: Int) throws -> Data? {
        guard let handle = try? FileHandle(forReadingFrom: url) else {
            return nil
        }
        defer {
            try? handle.close()
        }
        let data = try handle.read(upToCount: maximumBytes + 1) ?? Data()
        guard data.count <= maximumBytes else {
            throw RunnerError.requestTooLarge
        }
        return data
    }

    func executeSingleRequest(_ data: Data) throws -> Data {
        guard data.count <= maximumRequestBytes,
              let fields = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              fields["protocol_version"] as? Int == 1,
              let requestId = fields["request_id"] as? String,
              requestId.count <= 128,
              let command = fields["command"] as? String else {
            throw RunnerError.invalidRequestEnvelope
        }
        var activeBundleIdentifier =
            fields["initial_bundle_identifier"] as? String ?? springboardBundleIdentifier
        var shouldStop = false
        do {
            let result = try execute(
                command: command,
                fields: fields,
                activeBundleIdentifier: &activeBundleIdentifier,
                shouldStop: &shouldStop
            )
            return try response(requestId: requestId, result: result)
        } catch {
            return try response(requestId: requestId, error: String(describing: error))
        }
    }

    func run() throws {
        let listener = try makeListener()
        defer {
            close(listener)
        }

        var activeBundleIdentifier = springboardBundleIdentifier
        var shouldStop = false

        while !shouldStop {
            let client = accept(listener, nil, nil)
            guard client >= 0 else {
                throw socketError("accept")
            }
            try processClient(
                client,
                activeBundleIdentifier: &activeBundleIdentifier,
                shouldStop: &shouldStop
            )
        }
    }

    private func processClient(
        _ client: Int32,
        activeBundleIdentifier: inout String,
        shouldStop: inout Bool
    ) throws {
        defer {
            close(client)
        }

        var noSignal: Int32 = 1
        guard setsockopt(
            client,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            &noSignal,
            socklen_t(MemoryLayout<Int32>.size)
        ) == 0 else {
            throw socketError("setsockopt")
        }

        while !shouldStop {
            guard let requestData = try readRequest(client) else {
                return
            }

            let fields: [String: Any]
            do {
                let request = try JSONSerialization.jsonObject(with: requestData)
                guard let requestFields = request as? [String: Any] else {
                    throw RunnerError.invalidRequestEnvelope
                }
                fields = requestFields
            } catch {
                try sendResponse(
                    client,
                    response: response(requestId: "", error: "invalid JSON request")
                )
                continue
            }

            guard let requestId = fields["request_id"] as? String,
                  requestId.count <= 128,
                  let command = fields["command"] as? String,
                  fields["protocol_version"] as? Int == 1 else {
                try sendResponse(
                    client,
                    response: response(requestId: "", error: "invalid request envelope")
                )
                continue
            }

            do {
                let result = try execute(
                    command: command,
                    fields: fields,
                    activeBundleIdentifier: &activeBundleIdentifier,
                    shouldStop: &shouldStop
                )
                try sendResponse(
                    client,
                    response: response(requestId: requestId, result: result)
                )
            } catch {
                try sendResponse(
                    client,
                    response: response(
                        requestId: requestId,
                        error: String(describing: error)
                    )
                )
            }
        }
    }

    private func makeListener() throws -> Int32 {
        let listener = socket(AF_INET, SOCK_STREAM, 0)
        guard listener >= 0 else {
            throw socketError("socket")
        }

        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = port.bigEndian
        address.sin_addr = in_addr(s_addr: in_addr_t(INADDR_LOOPBACK).bigEndian)

        let bindResult = withUnsafePointer(to: &address) { addressPointer in
            addressPointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(listener, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard bindResult == 0 else {
            close(listener)
            throw socketError("bind")
        }
        guard listen(listener, 4) == 0 else {
            close(listener)
            throw socketError("listen")
        }
        return listener
    }

    private func readRequest(_ client: Int32) throws -> Data? {
        var request = Data()
        var finished = false

        while !finished {
            var buffer = [UInt8](repeating: 0, count: 4096)
            let readCount = buffer.withUnsafeMutableBytes { bufferPointer in
                recv(client, bufferPointer.baseAddress, bufferPointer.count, 0)
            }
            guard readCount >= 0 else {
                throw socketError("recv")
            }
            if readCount == 0 {
                if request.isEmpty {
                    return nil
                }
                throw RunnerError.incompleteRequest
            }

            let received = buffer.prefix(readCount)
            if let newline = received.firstIndex(of: 10) {
                guard received.index(after: newline) == received.endIndex else {
                    throw RunnerError.pipelinedRequests
                }
                request.append(contentsOf: received[..<newline])
                finished = true
            } else {
                request.append(contentsOf: received)
            }
            guard request.count <= maximumRequestBytes else {
                throw RunnerError.requestTooLarge
            }
        }

        return request
    }

    private func execute(
        command: String,
        fields: [String: Any],
        activeBundleIdentifier: inout String,
        shouldStop: inout Bool
    ) throws -> [String: Any] {
        switch command {
        case "health":
            return ["state": "ready"]
        case "launch":
            guard let bundleIdentifier = fields["bundle_identifier"] as? String,
                  !bundleIdentifier.isEmpty,
                  bundleIdentifier.count <= 255 else {
                throw RunnerError.invalidBundleIdentifier
            }
            activeBundleIdentifier = bundleIdentifier
            XCUIApplication(bundleIdentifier: bundleIdentifier).launch()
            return ["bundle_identifier": bundleIdentifier]
        case "tap":
            let coordinate = try coordinate(fields, bundleIdentifier: activeBundleIdentifier)
            coordinate.tap()
            return ["submitted": true]
        case "swipe":
            let start = try coordinate(
                fields,
                bundleIdentifier: activeBundleIdentifier,
                prefix: "start_"
            )
            let end = try coordinate(
                fields,
                bundleIdentifier: activeBundleIdentifier,
                prefix: "end_"
            )
            start.press(forDuration: 0.05, thenDragTo: end)
            return ["submitted": true]
        case "type":
            guard let text = fields["text"] as? String,
                  !text.isEmpty,
                  text.utf8.count <= maximumTextBytes else {
                throw RunnerError.invalidText
            }
            XCUIApplication(bundleIdentifier: activeBundleIdentifier).typeText(text)
            return ["submitted": true]
        case "accessibility":
            let requestedLimit = fields["max_elements"] as? Int ?? maximumAccessibilityElements
            guard (1...maximumAccessibilityElements).contains(requestedLimit) else {
                throw RunnerError.invalidAccessibilityLimit
            }
            return try accessibilitySnapshot(
                bundleIdentifier: activeBundleIdentifier,
                maximumElements: requestedLimit
            )
        case "shutdown":
            shouldStop = true
            return ["stopped": true]
        default:
            throw RunnerError.unknownCommand
        }
    }

    private func coordinate(
        _ fields: [String: Any],
        bundleIdentifier: String,
        prefix: String = ""
    ) throws -> XCUICoordinate {
        let horizontalKey = "\(prefix)x"
        let verticalKey = "\(prefix)y"
        guard let horizontal = fields[horizontalKey] as? Double,
              let vertical = fields[verticalKey] as? Double,
              horizontal.isFinite,
              vertical.isFinite,
              (0.0...1.0).contains(horizontal),
              (0.0...1.0).contains(vertical) else {
            throw RunnerError.invalidCoordinates
        }
        let application = XCUIApplication(bundleIdentifier: bundleIdentifier)
        return application.coordinate(
            withNormalizedOffset: CGVector(dx: horizontal, dy: vertical)
        )
    }

    private func accessibilitySnapshot(
        bundleIdentifier: String,
        maximumElements: Int
    ) throws -> [String: Any] {
        let application = XCUIApplication(bundleIdentifier: bundleIdentifier)
        let query = application.descendants(matching: .any)
        let elementCount = query.count
        let wasTruncated = elementCount > maximumElements
        var elements = [[String: Any]]()

        for index in 0..<min(elementCount, maximumElements) {
            let element = query.element(boundBy: index)
            let frame = element.frame
            var item: [String: Any] = [
                "role": String(describing: element.elementType),
                "enabled": element.isEnabled,
                "visible": element.isHittable,
                "frame": [
                    "x": frame.origin.x,
                    "y": frame.origin.y,
                    "width": frame.size.width,
                    "height": frame.size.height
                ]
            ]

            if !element.identifier.isEmpty {
                item["identifier"] = element.identifier
            }
            if !element.label.isEmpty {
                item["label"] = element.label
            }
            if element.elementType == .secureTextField {
                item["password"] = true
            } else if let value = element.value {
                item["value"] = String(describing: value)
            }
            elements.append(item)
        }

        let frame = application.frame
        return [
            "elements": elements,
            "truncated": wasTruncated,
            "application_frame": [
                "x": frame.origin.x,
                "y": frame.origin.y,
                "width": frame.size.width,
                "height": frame.size.height
            ]
        ]
    }

    private func response(
        requestId: String,
        result: [String: Any]? = nil,
        error: String? = nil
    ) throws -> Data {
        var value: [String: Any] = [
            "protocol_version": 1,
            "request_id": requestId,
            "ok": error == nil
        ]
        if let result {
            value["result"] = result
        }
        if let error {
            value["error"] = error
        }
        return try JSONSerialization.data(withJSONObject: value)
    }

    private func sendResponse(_ client: Int32, response: Data) throws {
        guard response.count <= maximumResponseBytes else {
            throw RunnerError.responseTooLarge
        }
        var bytes = response
        bytes.append(10)
        try bytes.withUnsafeBytes { bufferPointer in
            var offset = 0
            while offset < bufferPointer.count {
                let sentCount = send(
                    client,
                    bufferPointer.baseAddress?.advanced(by: offset),
                    bufferPointer.count - offset,
                    0
                )
                guard sentCount > 0 else {
                    throw socketError("send")
                }
                offset += sentCount
            }
        }
    }

    private func socketError(_ operation: String) -> NSError {
        NSError(
            domain: NSPOSIXErrorDomain,
            code: Int(errno),
            userInfo: [NSLocalizedDescriptionKey: "\(operation) failed"]
        )
    }
}

private enum RunnerError: Error {
    case requestTooLarge
    case responseTooLarge
    case invalidRequestEnvelope
    case incompleteRequest
    case pipelinedRequests
    case invalidBundleIdentifier
    case invalidText
    case invalidAccessibilityLimit
    case invalidCoordinates
    case unknownCommand
}
