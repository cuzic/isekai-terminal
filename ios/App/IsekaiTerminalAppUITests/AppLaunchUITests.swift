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

    /// SwiftUIのTextField/SecureFieldは`.tap()`直後にまだキーボードフォーカスが
    /// 確定していないことがあり、続けて`.typeText()`すると稀に「Neither element
    /// nor any descendant has keyboard focus」で失敗する(CI実行時に
    /// `testPasswordAuthProfileTapShowsPasswordPrompt`で実際に発生・確認済み。
    /// 他の全テストも同じ`tap()`直後`typeText()`パターンを使っており、今回たまたま
    /// このテストで顕在化しただけで、いつ他のテストで再発してもおかしくない)。
    /// `XCUIElement.hasKeyboardFocus`はこのプロジェクトのdeployment target(iOS 16)
    /// 向けビルドでは使えない(`error: value of type 'XCUIElement' has no member
    /// 'hasKeyboardFocus'`)ため、代わりにキーボード自体の出現をXCUITestの標準的な
    /// 手法(`app.keyboards.element`)で待つ。
    private func ensureFocus(_ field: XCUIElement, timeout: TimeInterval = 5) {
        XCTAssertTrue(field.waitForExistence(timeout: timeout))
        field.tap()
        if !XCUIApplication().keyboards.element.waitForExistence(timeout: 3) {
            field.tap() // 最後にもう一度だけリトライする
            // Codexレビュー指摘: リトライ後も再度キーボード出現を待たないと、
            // typeText()がフォーカス未確定のまま呼ばれてしまいレース対策として
            // 不十分だった。ここで明示的に失敗させる。
            XCTAssertTrue(XCUIApplication().keyboards.element.waitForExistence(timeout: 3))
        }
    }

    private func focusAndType(_ field: XCUIElement, _ text: String) {
        ensureFocus(field)
        field.typeText(text)
    }

    /// 新規行の保存/生成直後、`AppServices.shared`が実DB(タスク間でリセットされない、
    /// このファイル冒頭のコメント参照)を使い続けるため、テストを重ねるほどList内の
    /// 行数が増え、`displayName`昇順ソート次第では新規行が画面外に留まり得る
    /// (`ProfileDatabase.swift`の`.order(Column("displayName"))`参照)。SwiftUIの`List`は
    /// 画面外のセルをアクセシビリティツリーへ出さないことがあるため、
    /// `waitForExistence`だけでは検出できない(Codexレビュー指摘、
    /// `testAddProfileFlowCreatesNewProfileRow`の実際の失敗)。見つかるまでスクロールする。
    @discardableResult
    private func waitForRowVisible(_ label: String, app: XCUIApplication, scrollContainer: XCUIElement, timeout: TimeInterval = 10) -> XCUIElement {
        let row = app.staticTexts[label]
        let deadline = Date().addingTimeInterval(timeout)
        while !row.exists && Date() < deadline {
            scrollContainer.swipeUp()
        }
        XCTAssertTrue(row.waitForExistence(timeout: 2), "row \(label) never became visible even after scrolling")
        return row
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

        focusAndType(app.textFields["profileLabelField"], label)
        focusAndType(app.textFields["profileHostField"], "127.0.0.1")
        focusAndType(app.textFields["profileUsernameField"], "tester")

        app.buttons["saveProfileButton"].tap()

        waitForRowVisible(label, app: app, scrollContainer: app.collectionViews["profileList"])
    }

    func testDeleteProfileRemovesRow() throws {
        let app = XCUIApplication()
        app.launch()

        let label = "UITest-Delete-\(UUID().uuidString.prefix(8))"

        XCTAssertTrue(app.buttons["addProfileButton"].waitForExistence(timeout: 10))
        app.buttons["addProfileButton"].tap()

        focusAndType(app.textFields["profileLabelField"], label)
        focusAndType(app.textFields["profileHostField"], "127.0.0.1")
        focusAndType(app.textFields["profileUsernameField"], "tester")
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

        focusAndType(app.textFields["generateKeyLabelField"], label)

        app.buttons["confirmGenerateKeyButton"].tap()

        // 生成後の「鍵を生成しました」アラートを閉じる。sheetのdismissアニメーションと
        // このalertの提示が重ならないよう本番側(`KeyListView.swift`)を直したので、単純な
        // 存在待ち+tapで十分なはず。`isHittable`述語での追加待ち(`tapWhenHittable`)は
        // 一度試したが、alertボタンに対して`isHittable`評価自体が「Failed to determine
        // hittability ... Activation point invalid」という別の実際のCI失敗を起こしたため
        // 撤回した(根本原因は本番側の修正で解消済みのはずで、この評価はむしろ余計な
        // 失敗要因だった)。
        let dismissButton = app.alerts["鍵を生成しました"].buttons["閉じる"]
        XCTAssertTrue(dismissButton.waitForExistence(timeout: 10))
        dismissButton.tap()

        waitForRowVisible(label, app: app, scrollContainer: app.collectionViews["keyList"])
    }

    func testEditProfileFlowUpdatesRow() throws {
        let app = XCUIApplication()
        app.launch()

        let originalLabel = "UITest-Edit-\(UUID().uuidString.prefix(8))"
        let renamedLabel = "UITest-Edited-\(UUID().uuidString.prefix(8))"

        app.buttons["addProfileButton"].tap()
        focusAndType(app.textFields["profileLabelField"], originalLabel)
        focusAndType(app.textFields["profileHostField"], "127.0.0.1")
        focusAndType(app.textFields["profileUsernameField"], "tester")
        app.buttons["saveProfileButton"].tap()

        let originalRow = app.staticTexts[originalLabel]
        XCTAssertTrue(originalRow.waitForExistence(timeout: 5))

        originalRow.swipeLeft()
        let editSwipeButton = app.buttons["editProfileSwipeButton"]
        XCTAssertTrue(editSwipeButton.waitForExistence(timeout: 5))
        editSwipeButton.tap()

        let labelField = app.textFields["profileLabelField"]
        // 既存の値をクリアしてから新しいラベルを入力する
        // (タップ直後はカーソルが末尾付近にある前提でbackspaceを繰り返す、
        // XCUITestでテキストフィールドをクリアする定番の方法)。
        ensureFocus(labelField)
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

        focusAndType(app.textFields["keyImportLabelField"], label)

        // TextField(axis: .vertical)がtextFields/textViewsのどちらでアクセシビリティ
        // 公開されるか(iOSバージョンにより異なりうる)不確定なため、要素種別を問わない
        // クエリで探す。
        let pasteField = app.descendants(matching: .any)["keyImportPasteField"]
        focusAndType(pasteField, "-----BEGIN OPENSSH PRIVATE KEY-----\ndummy-for-ui-test\n-----END OPENSSH PRIVATE KEY-----\n")

        app.buttons["saveImportedKeyButton"].tap()

        XCTAssertTrue(app.staticTexts[label].waitForExistence(timeout: 5))
    }

    func testPasswordAuthProfileTapShowsPasswordPrompt() throws {
        let app = XCUIApplication()
        app.launch()

        let label = "UITest-PwPrompt-\(UUID().uuidString.prefix(8))"

        app.buttons["addProfileButton"].tap()
        focusAndType(app.textFields["profileLabelField"], label)
        focusAndType(app.textFields["profileHostField"], "127.0.0.1")
        focusAndType(app.textFields["profileUsernameField"], "tester")
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
