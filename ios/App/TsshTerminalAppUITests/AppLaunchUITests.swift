import XCTest

/// Phase 1D: `TsshTerminalApp`を実際にiOS Simulator上で起動し、タップ・文字入力・
/// スワイプ・メニュー操作・アラート確認といった、ユニットテスト(XCTestCaseの直接
/// メソッド呼び出し)では検証できないSimulator固有の挙動を検証する。
///
/// `AppServices.shared`は実ファイル(GRDB DB・Keychain)を使うシングルトンで
/// テスト間でリセットされないため、各テストは`UUID`を使ったユニークなラベルで
/// 新規行を識別する(既存データの有無を前提にしない)。
final class AppLaunchUITests: XCTestCase {
    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    func testAppLaunchesToProfileList() throws {
        let app = XCUIApplication()
        app.launch()

        let profileList = app.collectionViews["profileList"].firstMatch
        XCTAssertTrue(profileList.waitForExistence(timeout: 10))

        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = "profile-list-launch"
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testAddProfileFlowCreatesNewProfileRow() throws {
        let app = XCUIApplication()
        app.launch()

        let label = "UITest-\(UUID().uuidString.prefix(8))"

        XCTAssertTrue(app.buttons["addProfileButton"].waitForExistence(timeout: 10))
        app.buttons["addProfileButton"].tap()

        let labelField = app.textFields["profileLabelField"]
        XCTAssertTrue(labelField.waitForExistence(timeout: 5))
        labelField.tap()
        labelField.typeText(label)

        let hostField = app.textFields["profileHostField"]
        hostField.tap()
        hostField.typeText("127.0.0.1")

        let usernameField = app.textFields["profileUsernameField"]
        usernameField.tap()
        usernameField.typeText("tester")

        app.buttons["saveProfileButton"].tap()

        let newRow = app.staticTexts[label]
        XCTAssertTrue(newRow.waitForExistence(timeout: 5))
    }

    func testDeleteProfileRemovesRow() throws {
        let app = XCUIApplication()
        app.launch()

        let label = "UITest-Delete-\(UUID().uuidString.prefix(8))"

        XCTAssertTrue(app.buttons["addProfileButton"].waitForExistence(timeout: 10))
        app.buttons["addProfileButton"].tap()

        app.textFields["profileLabelField"].tap()
        app.textFields["profileLabelField"].typeText(label)
        app.textFields["profileHostField"].tap()
        app.textFields["profileHostField"].typeText("127.0.0.1")
        app.textFields["profileUsernameField"].tap()
        app.textFields["profileUsernameField"].typeText("tester")
        app.buttons["saveProfileButton"].tap()

        let row = app.staticTexts[label]
        XCTAssertTrue(row.waitForExistence(timeout: 5))

        row.swipeLeft()
        app.buttons["削除"].firstMatch.tap()

        let confirmButton = app.alerts["削除確認"].buttons["削除"]
        XCTAssertTrue(confirmButton.waitForExistence(timeout: 5))
        confirmButton.tap()

        XCTAssertFalse(row.waitForExistence(timeout: 5))
    }

    func testKeyGenerationFlowCreatesNewKeyRow() throws {
        let app = XCUIApplication()
        app.launch()

        let label = "UITest-Key-\(UUID().uuidString.prefix(8))"

        XCTAssertTrue(app.buttons["profileListMenu"].waitForExistence(timeout: 10))
        app.buttons["profileListMenu"].tap()

        let manageKeysItem = app.buttons["manageKeysMenuItem"]
        XCTAssertTrue(manageKeysItem.waitForExistence(timeout: 5))
        manageKeysItem.tap()

        XCTAssertTrue(app.buttons["generateKeyButton"].waitForExistence(timeout: 5))
        app.buttons["generateKeyButton"].tap()

        let generateLabelField = app.textFields["generateKeyLabelField"]
        XCTAssertTrue(generateLabelField.waitForExistence(timeout: 5))
        generateLabelField.tap()
        generateLabelField.typeText(label)

        app.buttons["confirmGenerateKeyButton"].tap()

        // 生成後の「鍵を生成しました」アラートを閉じる。
        let dismissButton = app.alerts["鍵を生成しました"].buttons["閉じる"]
        XCTAssertTrue(dismissButton.waitForExistence(timeout: 5))
        dismissButton.tap()

        let newKeyRow = app.staticTexts[label]
        XCTAssertTrue(newKeyRow.waitForExistence(timeout: 5))
    }
}
