import XCTest
@testable import IsekaiTerminalCoreLogic

/// タスク#3: Android版`TerminalTabsViewModelTest`の`maybeEnsureTmuxTabWindow`関連
/// テストと対称。実際のtmux/Rustには触れず、`TmuxTabWindowResolving`/
/// `ClientIdentityStore`/`TmuxTabLocatorStore`をすべてフェイクに差し替えて、
/// このファイルが持つ「薄い配線」ロジック(clientId取得→existingTag読み出し→
/// resolver呼び出し→tag書き戻し→ラベル組み立て)だけを検証する。
final class TmuxTabWindowCoordinatorTests: XCTestCase {
    private final class FakeClientIdentityStore: ClientIdentityStore {
        var stored: String?
        init(stored: String? = nil) { self.stored = stored }
        func readClientId() -> String? { stored }
        func writeClientId(_ value: String) { stored = value }
    }

    private final class FakeTmuxTabLocatorStore: TmuxTabLocatorStore {
        var tagsByProfileId: [Int64: String] = [:]
        var saveCallCount = 0

        func findTag(forProfileId profileId: Int64) throws -> String? {
            tagsByProfileId[profileId]
        }

        func saveTag(_ tag: String, forProfileId profileId: Int64) throws {
            saveCallCount += 1
            tagsByProfileId[profileId] = tag
        }
    }

    /// Rust側`ensure_tab_window`の代わりに、渡された引数をそのまま記録して固定の
    /// `TmuxTabWindowInfo`を返すフェイク。「新規タブは新しいタグを返す」
    /// 「reconnectするタブは既存タグを再利用する」という2つのケースをテストが
    /// 呼び出し側で作り分けられるよう、クロージャで応答を差し替えられるようにする。
    private final class FakeResolver: TmuxTabWindowResolving {
        var lastProfileIdentity: String?
        var lastClientId: String?
        var lastExistingTag: String?
        var callCount = 0
        var respond: (String, String, String?) -> TmuxTabWindowInfo

        init(respond: @escaping (String, String, String?) -> TmuxTabWindowInfo) {
            self.respond = respond
        }

        func ensureTmuxTabWindow(
            profileIdentity: String,
            clientId: String,
            existingTag: String?,
            enableNotifications: Bool
        ) async throws -> TmuxTabWindowInfo {
            callCount += 1
            lastProfileIdentity = profileIdentity
            lastClientId = clientId
            lastExistingTag = existingTag
            return respond(profileIdentity, clientId, existingTag)
        }
    }

    private func makeInfo(tag: String, windowIndex: UInt32, isNewWindow: Bool) -> TmuxTabWindowInfo {
        TmuxTabWindowInfo(
            tag: tag,
            windowIndex: windowIndex,
            sessionName: "session-for-\(tag)",
            groupName: "group-for-\(tag)",
            isNewWindow: isNewWindow
        )
    }

    // MARK: - 新規タブ

    func testNewTabGetsFreshWindowAndPersistsReturnedTag() async throws {
        let clientIdentityStore = FakeClientIdentityStore()
        let locatorStore = FakeTmuxTabLocatorStore()
        let resolver = FakeResolver { _, _, existingTag in
            XCTAssertNil(existingTag)
            return self.makeInfo(tag: "new-tag-1", windowIndex: 0, isNewWindow: true)
        }

        let result = try await TmuxTabWindowCoordinator.ensureWindow(
            profileId: 42,
            resolver: resolver,
            clientIdentityStore: clientIdentityStore,
            locatorStore: locatorStore
        )

        XCTAssertEqual(result.label, "tmux:0")
        XCTAssertTrue(result.info.isNewWindow)
        XCTAssertEqual(resolver.lastProfileIdentity, "profile:42")
        XCTAssertEqual(locatorStore.tagsByProfileId[42], "new-tag-1")
        XCTAssertEqual(locatorStore.saveCallCount, 1)
    }

    // MARK: - 再接続

    func testReconnectingTabReusesPersistedTag() async throws {
        let clientIdentityStore = FakeClientIdentityStore(stored: "existing-client-id")
        let locatorStore = FakeTmuxTabLocatorStore()
        locatorStore.tagsByProfileId[7] = "persisted-tag"
        let resolver = FakeResolver { _, _, existingTag in
            self.makeInfo(tag: existingTag ?? "unexpected", windowIndex: 3, isNewWindow: false)
        }

        let result = try await TmuxTabWindowCoordinator.ensureWindow(
            profileId: 7,
            resolver: resolver,
            clientIdentityStore: clientIdentityStore,
            locatorStore: locatorStore
        )

        XCTAssertEqual(resolver.lastExistingTag, "persisted-tag")
        XCTAssertEqual(resolver.lastClientId, "existing-client-id")
        XCTAssertFalse(result.info.isNewWindow)
        XCTAssertEqual(result.label, "tmux:3")
    }

    // MARK: - 2つのタブ(同じprofileId)が独立したタグを取得する

    func testTwoTabsOnSameProfileGetIndependentTagsAcrossSeparateCalls() async throws {
        let clientIdentityStore = FakeClientIdentityStore(stored: "shared-client-id")
        let locatorStore = FakeTmuxTabLocatorStore()
        var nextTagIndex = 0
        let resolver = FakeResolver { _, _, existingTag in
            // 呼び出し都度、新しいウィンドウを作ったかのように振る舞う(実際のtmux側の
            // 「新規ウィンドウ作成」相当。Rust側の実際の重複排除ロジックはここでは
            // テストしない — このテストの関心は「コーディネーターが呼び出しごとに
            // 独立してresolver/storeへ橋渡しできるか」のみ)。
            defer { nextTagIndex += 1 }
            return self.makeInfo(tag: "tab-tag-\(nextTagIndex)", windowIndex: UInt32(nextTagIndex), isNewWindow: true)
        }

        let firstTabResult = try await TmuxTabWindowCoordinator.ensureWindow(
            profileId: 99,
            resolver: resolver,
            clientIdentityStore: clientIdentityStore,
            locatorStore: locatorStore
        )
        // 2つ目のタブは「まだこのタブ自身の永続化済みタグが無い」状態を模すため、
        // 別のlocatorStoreインスタンスを使う(Android版で言う「まだ一度も
        // 接続したことがない別タブ」に相当)。
        let secondLocatorStore = FakeTmuxTabLocatorStore()
        let secondTabResult = try await TmuxTabWindowCoordinator.ensureWindow(
            profileId: 99,
            resolver: resolver,
            clientIdentityStore: clientIdentityStore,
            locatorStore: secondLocatorStore
        )

        XCTAssertNotEqual(firstTabResult.info.tag, secondTabResult.info.tag)
        XCTAssertEqual(resolver.callCount, 2)
    }

    // MARK: - アプリ再起動後の永続化(フェイク実装、実UserDefaults/GRDBは使わない)

    func testClientIdIsGeneratedOnceAndReusedAcrossSimulatedAppRestarts() async throws {
        let clientIdentityStore = FakeClientIdentityStore()
        let locatorStore = FakeTmuxTabLocatorStore()
        var generatedIds: [String] = []
        let resolver = FakeResolver { _, clientId, _ in
            generatedIds.append(clientId)
            return self.makeInfo(tag: "tag", windowIndex: 0, isNewWindow: true)
        }

        // 1回目の呼び出し(初回起動): clientIdがまだ無いので生成される。
        _ = try await TmuxTabWindowCoordinator.ensureWindow(
            profileId: 1,
            resolver: resolver,
            clientIdentityStore: clientIdentityStore,
            locatorStore: locatorStore
        )
        // 「アプリ再起動」を、同じ(永続化済みの)clientIdentityStoreインスタンスを
        // 使い回すことでシミュレートする(実アプリではUserDefaultsが同じ役割を果たす)。
        _ = try await TmuxTabWindowCoordinator.ensureWindow(
            profileId: 1,
            resolver: resolver,
            clientIdentityStore: clientIdentityStore,
            locatorStore: locatorStore
        )

        XCTAssertEqual(generatedIds.count, 2)
        XCTAssertEqual(generatedIds[0], generatedIds[1])
        XCTAssertEqual(clientIdentityStore.stored, generatedIds[0])
    }

    func testEnsureWindowPropagatesResolverFailureWithoutPersistingTag() async throws {
        let clientIdentityStore = FakeClientIdentityStore()
        let locatorStore = FakeTmuxTabLocatorStore()
        let resolver = FakeResolverThatThrows()

        do {
            _ = try await TmuxTabWindowCoordinator.ensureWindow(
                profileId: 5,
                resolver: resolver,
                clientIdentityStore: clientIdentityStore,
                locatorStore: locatorStore
            )
            XCTFail("Expected an error to be thrown")
        } catch {
            // 期待通り: resolverが失敗した場合、tagは書き戻されない。
        }
        XCTAssertEqual(locatorStore.saveCallCount, 0)
    }

    private final class FakeResolverThatThrows: TmuxTabWindowResolving {
        struct Failure: Error {}
        func ensureTmuxTabWindow(profileIdentity: String, clientId: String, existingTag: String?, enableNotifications: Bool) async throws -> TmuxTabWindowInfo {
            throw Failure()
        }
    }
}
