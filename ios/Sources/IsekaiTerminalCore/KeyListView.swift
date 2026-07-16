import SwiftUI
import UIKit
import IsekaiTerminalCoreLogic

/// Phase 1D: Android版`KeyListScreen.kt`/`KeyListViewModel.kt`の移植。
/// 鍵一覧・削除・ed25519鍵生成をサポートする(既存鍵のインポートは`KeyImportView`)。
@MainActor
public final class KeyListModel: ObservableObject {
    @Published public private(set) var keys: [KeyEntry] = []
    @Published public var pendingDelete: KeyEntry?
    @Published public var generatedPublicKey: String?
    @Published public var isGenerating = false
    @Published public var generateError: String?

    private let db: ProfileDatabase
    private let vault: CredentialVault

    public init(db: ProfileDatabase = AppServices.shared.db, vault: CredentialVault = AppServices.shared.vault) {
        self.db = db
        self.vault = vault
    }

    public func load() {
        keys = (try? db.fetchAllKeyEntries()) ?? []
    }

    public func requestDelete(_ key: KeyEntry) { pendingDelete = key }
    public func dismissDelete() { pendingDelete = nil }

    public func confirmDelete(_ key: KeyEntry) {
        pendingDelete = nil
        try? db.deleteKeyEntry(id: key.id)
        try? vault.delete(keyId: key.id)
        load()
    }

    public func generateKey(displayName: String) {
        guard !isGenerating else { return }
        isGenerating = true
        generateError = nil
        defer { isGenerating = false }

        let (pemBytes, authorizedKeysLine) = KeyManager.generateEd25519Pair()
        let keyId = UUID().uuidString
        let metadata = CredentialVault.Metadata(keyId: keyId, keyType: "ed25519", publicKey: authorizedKeysLine)
        do {
            try vault.store(secret: pemBytes, metadata: metadata)
            try db.insert(keyEntry: KeyEntry(
                id: keyId,
                displayName: displayName,
                keyType: "ed25519",
                publicKey: authorizedKeysLine
            ))
            generatedPublicKey = authorizedKeysLine
            load()
        } catch {
            generateError = "生成失敗: \(error)"
        }
    }

    public func dismissGeneratedPublicKey() { generatedPublicKey = nil }
}

public struct KeyListView: View {
    @StateObject private var model: KeyListModel
    private let onImportKey: () -> Void

    @State private var showGenerateSheet = false
    @State private var generateLabel = ""
    /// 「鍵を生成しました」alertを表示するかどうか。`model.generatedPublicKey != nil`を
    /// 直接alertのisPresentedへ結び付けていた旧実装は、生成ボタンの1アクション内で
    /// `showGenerateSheet = false`(sheet dismiss)と`generatedPublicKey`セット(alert present
    /// 条件成立)が同時に起き、SwiftUIが同一更新サイクル内でのsheet dismiss中の新規alert
    /// 提示を安定して扱えず、CIのXCUITestで「Failed to get matching snapshot」のflaky失敗を
    /// 起こしていた(Codexレビュー指摘、`AppLaunchUITests.testKeyGenerationFlowCreatesNewKeyRow`
    /// で再現)。sheetの`onDismiss`完了後にこのフラグを立てることで、2つのpresentationが
    /// 重ならないようにする。
    @State private var showGeneratedAlert = false

    // `model`にデフォルト値を持たせられない理由は`ProfileListView.init`のコメント参照。
    public init(model: KeyListModel, onImportKey: @escaping () -> Void) {
        _model = StateObject(wrappedValue: model)
        self.onImportKey = onImportKey
    }

    public var body: some View {
        List {
            if model.keys.isEmpty {
                Text("「＋」でインポート、「生成」で新規作成")
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("keyListEmptyHint")
            }
            ForEach(model.keys, id: \.id) { key in
                VStack(alignment: .leading, spacing: 4) {
                    Text(key.displayName).font(.headline)
                    Text(key.publicKey)
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
                .accessibilityIdentifier("keyRow_\(key.id)")
                .swipeActions {
                    Button("削除", role: .destructive) { model.requestDelete(key) }
                    Button("コピー") { UIPasteboard.general.string = key.publicKey }.tint(.blue)
                }
            }
        }
        .accessibilityIdentifier("keyList")
        .navigationTitle("鍵一覧")
        .toolbar {
            ToolbarItem(placement: .navigationBarTrailing) {
                Button("生成") { generateLabel = ""; showGenerateSheet = true }
                    .accessibilityIdentifier("generateKeyButton")
            }
            ToolbarItem(placement: .navigationBarTrailing) {
                Button(action: onImportKey) {
                    Image(systemName: "plus")
                }
                .accessibilityIdentifier("importKeyButton")
            }
        }
        .onAppear { model.load() }
        .alert(
            "鍵を削除",
            isPresented: Binding(
                get: { model.pendingDelete != nil },
                set: { if !$0 { model.dismissDelete() } }
            )
        ) {
            Button("キャンセル", role: .cancel) { model.dismissDelete() }
            Button("削除", role: .destructive) {
                if let key = model.pendingDelete { model.confirmDelete(key) }
            }
        } message: {
            Text("「\(model.pendingDelete?.displayName ?? "")」を削除しますか？この操作は元に戻せません。")
        }
        .sheet(isPresented: $showGenerateSheet, onDismiss: {
            if model.generatedPublicKey != nil {
                showGeneratedAlert = true
            }
        }) {
            NavigationStack {
                Form {
                    TextField("ラベル", text: $generateLabel)
                        .accessibilityIdentifier("generateKeyLabelField")
                    if let error = model.generateError {
                        Text(error).foregroundStyle(.red)
                    }
                }
                .navigationTitle("ed25519鍵を生成")
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("キャンセル") { showGenerateSheet = false }
                    }
                    ToolbarItem(placement: .confirmationAction) {
                        Button(model.isGenerating ? "生成中…" : "生成") {
                            model.generateKey(displayName: generateLabel)
                            if model.generateError == nil { showGenerateSheet = false }
                        }
                        .disabled(model.isGenerating || generateLabel.trimmingCharacters(in: .whitespaces).isEmpty)
                        .accessibilityIdentifier("confirmGenerateKeyButton")
                    }
                }
            }
        }
        .alert(
            "鍵を生成しました",
            isPresented: $showGeneratedAlert
        ) {
            Button("コピーして閉じる") {
                if let pub = model.generatedPublicKey { UIPasteboard.general.string = pub }
                model.dismissGeneratedPublicKey()
            }
            Button("閉じる", role: .cancel) { model.dismissGeneratedPublicKey() }
        } message: {
            Text("以下の公開鍵をサーバーの ~/.ssh/authorized_keys に追加してください。\n\(model.generatedPublicKey ?? "")")
        }
    }
}
