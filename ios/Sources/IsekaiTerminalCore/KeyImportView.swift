import SwiftUI
import UniformTypeIdentifiers
import IsekaiTerminalCoreLogic

/// Phase 1D: Android版`KeyImportScreen.kt`/`KeyImportViewModel.kt`の移植。
/// Android版のSAFファイルピッカーに対応するのは`.fileImporter`。貼り付けでの
/// インポートも合わせて提供する(Androidには無いがiOSでは一般的なUX)。
/// 既存鍵からの公開鍵抽出はAndroid版でも行っていない(`KeyManager.extractPublicKeyHint`
/// 参照)ため、iOS版でも同様にプレースホルダー文言を保存する。
@MainActor
public final class KeyImportModel: ObservableObject {
    @Published public var isSaving = false
    @Published public var errorMessage: String?

    private let db: ProfileDatabase
    private let vault: CredentialVault

    public init(db: ProfileDatabase = AppServices.shared.db, vault: CredentialVault = AppServices.shared.vault) {
        self.db = db
        self.vault = vault
    }

    public func importKey(pemBytes: Data, displayName: String) -> Bool {
        guard !isSaving else { return false }
        errorMessage = nil
        guard !displayName.trimmingCharacters(in: .whitespaces).isEmpty else {
            errorMessage = "ラベルを入力してください"
            return false
        }
        guard !pemBytes.isEmpty else {
            errorMessage = "秘密鍵を選択、または貼り付けてください"
            return false
        }
        isSaving = true
        defer { isSaving = false }

        let keyId = UUID().uuidString
        let hint = KeyManager.extractPublicKeyHint(pemBytes: pemBytes)
        let metadata = CredentialVault.Metadata(keyId: keyId, keyType: "imported", publicKey: hint)
        do {
            try vault.store(secret: pemBytes, metadata: metadata)
            try db.insert(keyEntry: KeyEntry(
                id: keyId,
                displayName: displayName,
                keyType: "imported",
                publicKey: hint
            ))
            return true
        } catch {
            errorMessage = "保存に失敗しました: \(error)"
            return false
        }
    }
}

public struct KeyImportView: View {
    @StateObject private var model: KeyImportModel
    private let onSave: () -> Void
    private let onCancel: () -> Void

    @State private var displayName = ""
    @State private var pastedPem = ""
    @State private var selectedFileName: String?
    @State private var showFileImporter = false

    // `model`にデフォルト値を持たせられない理由は`ProfileListView.init`のコメント参照。
    public init(model: KeyImportModel, onSave: @escaping () -> Void, onCancel: @escaping () -> Void) {
        _model = StateObject(wrappedValue: model)
        self.onSave = onSave
        self.onCancel = onCancel
    }

    public var body: some View {
        Form {
            Section("秘密鍵をインポート") {
                TextField("ラベル", text: $displayName)
                    .accessibilityIdentifier("keyImportLabelField")

                Button("ファイルを選択") { showFileImporter = true }
                    .accessibilityIdentifier("keyImportFilePickerButton")
                if let name = selectedFileName {
                    Text("選択中: \(name)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                TextField("または秘密鍵の内容を貼り付け", text: $pastedPem, axis: .vertical)
                    .font(.system(.caption, design: .monospaced))
                    .lineLimit(4...10)
                    .accessibilityIdentifier("keyImportPasteField")
            }

            if let error = model.errorMessage {
                Section {
                    Text(error).foregroundStyle(.red)
                        .accessibilityIdentifier("keyImportError")
                }
            }
        }
        .navigationTitle("秘密鍵をインポート")
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                Button("キャンセル", action: onCancel)
            }
            ToolbarItem(placement: .confirmationAction) {
                Button(model.isSaving ? "保存中…" : "保存") {
                    let bytes = Data(pastedPem.utf8)
                    if model.importKey(pemBytes: bytes, displayName: displayName) {
                        onSave()
                    }
                }
                .disabled(model.isSaving)
                .accessibilityIdentifier("saveImportedKeyButton")
            }
        }
        .fileImporter(isPresented: $showFileImporter, allowedContentTypes: [.item]) { result in
            switch result {
            case .success(let url):
                selectedFileName = url.lastPathComponent
                let didAccess = url.startAccessingSecurityScopedResource()
                defer { if didAccess { url.stopAccessingSecurityScopedResource() } }
                if let data = try? Data(contentsOf: url), let text = String(data: data, encoding: .utf8) {
                    pastedPem = text
                }
            case .failure:
                selectedFileName = nil
            }
        }
    }
}
