import Foundation
import XCTest

final class MoltisUITests: XCTestCase {
    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    func testCanAddEnvironmentVariableFromSettings() throws {
        let runtimeRoot = try makeRuntimeRoot()
        defer {
            try? FileManager.default.removeItem(at: runtimeRoot)
        }

        let configDir = runtimeRoot.appendingPathComponent("config", isDirectory: true)
        let dataDir = runtimeRoot.appendingPathComponent("data", isDirectory: true)
        try FileManager.default.createDirectory(at: configDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: dataDir, withIntermediateDirectories: true)

        let app = XCUIApplication()
        app.launchEnvironment["MOLTIS_UI_TEST_SKIP_ONBOARDING"] = "1"
        app.launchEnvironment["MOLTIS_CONFIG_DIR"] = configDir.path
        app.launchEnvironment["MOLTIS_DATA_DIR"] = dataDir.path
        app.launch()

        let openSettingsButton = app.buttons["open-settings-button"]
        XCTAssertTrue(openSettingsButton.waitForExistence(timeout: 20))
        openSettingsButton.click()

        let settingsWindow = app.windows["Settings"].firstMatch
        XCTAssertTrue(settingsWindow.waitForExistence(timeout: 20))

        let environmentSection = settingsWindow
            .descendants(matching: .any)
            .matching(identifier: "settings-section-environment")
            .firstMatch
        XCTAssertTrue(environmentSection.waitForExistence(timeout: 10))
        environmentSection.click()

        let envKey = "MOLTIS_UI_TEST_\(UUID().uuidString.replacingOccurrences(of: "-", with: "_"))"

        let keyField = settingsWindow.textFields["settings-env-add-key"]
        XCTAssertTrue(keyField.waitForExistence(timeout: 10))
        clearAndType(text: envKey, into: keyField)

        let valueField = settingsWindow.secureTextFields["settings-env-add-value"]
        XCTAssertTrue(valueField.waitForExistence(timeout: 10))
        clearAndType(text: "ui-test-value", into: valueField)

        let addButton = settingsWindow.buttons["settings-env-add-button"]
        XCTAssertTrue(addButton.waitForExistence(timeout: 10))
        addButton.click()

        let successMessage = settingsWindow.staticTexts["settings-env-message"]
        XCTAssertTrue(successMessage.waitForExistence(timeout: 20))

        let addedKeyLabel = settingsWindow.staticTexts[envKey]
        XCTAssertTrue(addedKeyLabel.waitForExistence(timeout: 20))
    }
}

private extension MoltisUITests {
    func makeRuntimeRoot() throws -> URL {
        let root = URL(fileURLWithPath: NSTemporaryDirectory(), isDirectory: true)
            .appendingPathComponent("moltis-ui-tests-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        return root
    }

    func clearAndType(text: String, into element: XCUIElement) {
        element.click()
        if let existingValue = element.value as? String, !existingValue.isEmpty {
            let deleteSequence = String(
                repeating: XCUIKeyboardKey.delete.rawValue,
                count: existingValue.count
            )
            element.typeText(deleteSequence)
        }
        element.typeText(text)
    }
}
