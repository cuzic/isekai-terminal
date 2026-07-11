import XCTest

/// Phase 1D: `IsekaiTerminalApp`を実際にiOS Simulator上で起動し、タップ・文字入力・
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
        let deleteSwipeButton = app.buttons["deleteProfileSwipeButton"]
        XCTAssertTrue(deleteSwipeButton.waitForExistence(timeout: 5))
        deleteSwipeButton.tap()

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

    func testEditProfileFlowUpdatesRow() throws {
        let app = XCUIApplication()
        app.launch()

        let originalLabel = "UITest-Edit-\(UUID().uuidString.prefix(8))"
        let renamedLabel = "UITest-Edited-\(UUID().uuidString.prefix(8))"

        app.buttons["addProfileButton"].tap()
        app.textFields["profileLabelField"].tap()
        app.textFields["profileLabelField"].typeText(originalLabel)
        app.textFields["profileHostField"].tap()
        app.textFields["profileHostField"].typeText("127.0.0.1")
        app.textFields["profileUsernameField"].tap()
        app.textFields["profileUsernameField"].typeText("tester")
        app.buttons["saveProfileButton"].tap()

        let originalRow = app.staticTexts[originalLabel]
        XCTAssertTrue(originalRow.waitForExistence(timeout: 5))

        originalRow.swipeLeft()
        let editSwipeButton = app.buttons["editProfileSwipeButton"]
        XCTAssertTrue(editSwipeButton.waitForExistence(timeout: 5))
        editSwipeButton.tap()

        let labelField = app.textFields["profileLabelField"]
        XCTAssertTrue(labelField.waitForExistence(timeout: 5))
        // 既存の値をクリアしてから新しいラベルを入力する
        // (タップ直後はカーソルが末尾付近にある前提でbackspaceを繰り返す、
        // XCUITestでテキストフィールドをクリアする定番の方法)。
        labelField.tap()
        if let existing = labelField.value as? String {
            labelField.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: existing.count))
        }
        labelField.typeText(renamedLabel)
        app.buttons["saveProfileButton"].tap()

        XCTAssertTrue(app.staticTexts[renamedLabel].waitForExistence(timeout: 5))
        XCTAssertFalse(app.staticTexts[originalLabel].exists)
    }

    func testKeyImportFlowViaPasteCreatesNewKeyRow() throws {
        let app = XCUIApplication()
        app.launch()

        let label = "UITest-Import-\(UUID().uuidString.prefix(8))"

        app.buttons["profileListMenu"].tap()
        app.buttons["manageKeysMenuItem"].tap()

        XCTAssertTrue(app.buttons["importKeyButton"].waitForExistence(timeout: 5))
        app.buttons["importKeyButton"].tap()

        let importLabelField = app.textFields["keyImportLabelField"]
        XCTAssertTrue(importLabelField.waitForExistence(timeout: 5))
        importLabelField.tap()
        importLabelField.typeText(label)

        // TextField(axis: .vertical)がtextFields/textViewsのどちらでアクセシビリティ
        // 公開されるか(iOSバージョンにより異なりうる)不確定なため、要素種別を問わない
        // クエリで探す。
        let pasteField = app.descendants(matching: .any)["keyImportPasteField"]
        XCTAssertTrue(pasteField.waitForExistence(timeout: 5))
        pasteField.tap()
        pasteField.typeText("-----BEGIN OPENSSH PRIVATE KEY-----\ndummy-for-ui-test\n-----END OPENSSH PRIVATE KEY-----\n")

        app.buttons["saveImportedKeyButton"].tap()

        XCTAssertTrue(app.staticTexts[label].waitForExistence(timeout: 5))
    }

    func testPasswordAuthProfileTapShowsPasswordPrompt() throws {
        let app = XCUIApplication()
        app.launch()

        let label = "UITest-PwPrompt-\(UUID().uuidString.prefix(8))"

        app.buttons["addProfileButton"].tap()
        app.textFields["profileLabelField"].tap()
        app.textFields["profileLabelField"].typeText(label)
        app.textFields["profileHostField"].tap()
        app.textFields["profileHostField"].typeText("127.0.0.1")
        app.textFields["profileUsernameField"].tap()
        app.textFields["profileUsernameField"].typeText("tester")
        // 認証方式は既定でパスワード(鍵は選択しない)。
        app.buttons["saveProfileButton"].tap()

        let row = app.staticTexts[label]
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        row.tap()

        let passwordField = app.secureTextFields["passwordField"]
        XCTAssertTrue(passwordField.waitForExistence(timeout: 5))

        // ターミナル本画面は未実装のため、ここではダイアログの出現だけ確認しキャンセルする。
        app.navigationBars.buttons["キャンセル"].firstMatch.tap()
        XCTAssertFalse(passwordField.exists)
    }

    /// Epic M以降に追加された4つのオプトイン設定トグル(画面の保護/リモートクリップボード
    /// 書込・送信許可/tmux迂回control-plane)が、メニューから実際にON/OFFを切り替えられる
    /// ことを確認する(`ScreenProtectionOverlay`/`RemoteClipboardBridge`/
    /// `CtlSocketForwardSettings`が読む`@AppStorage`との配線確認)。
    func testOptInSettingsMenuItemsToggleBetweenOnAndOff() throws {
        let app = XCUIApplication()
        app.launch()

        let menuItems = [
            "screenProtectionMenuItem",
            "remoteClipboardWriteMenuItem",
            "remoteClipboardPullMenuItem",
            "ctlSocketForwardMenuItem",
        ]

        for identifier in menuItems {
            XCTAssertTrue(app.buttons["profileListMenu"].waitForExistence(timeout: 10))
            app.buttons["profileListMenu"].tap()

            let item = app.buttons[identifier]
            XCTAssertTrue(item.waitForExistence(timeout: 5))
            let initiallyOff = item.label.hasSuffix("OFF")
            item.tap()

            app.buttons["profileListMenu"].tap()
            let itemAfterToggle = app.buttons[identifier]
            XCTAssertTrue(itemAfterToggle.waitForExistence(timeout: 5))
            XCTAssertEqual(itemAfterToggle.label.hasSuffix("OFF"), !initiallyOff)

            // 次のトグルの検証に影響しないよう、必ず元の状態(OFF)へ戻す
            // (これによりメニューも閉じるので、次のループ先頭のtapで開き直す)。
            itemAfterToggle.tap()
        }
    }
}
