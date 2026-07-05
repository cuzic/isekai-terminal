import SwiftUI
import TsshCoreLogic

/// Phase 1G-1(#53): Android版`SnippetEditScreen.kt`/`SnippetEditViewModel.kt`の移植。
@MainActor
public final class SnippetEditModel: ObservableObject {
    @Published public var label: String
    @Published public var command: String
    @Published public var appendNewline: Bool
    @Published public var profileId: Int64?
    @Published public var availableProfiles: [ConnectionProfile] = []
    @Published public var errorMessage: String?

    private let db: ProfileDatabase
    private let existingId: Int64?
    private let existingSortOrder: Int

    public init(snippet: Snippet?, db: ProfileDatabase = AppServices.shared.db) {
        self.db = db
        self.existingId = snippet?.id
        self.existingSortOrder = snippet?.sortOrder ?? 0
        self.label = snippet?.label ?? ""
        self.command = snippet?.command ?? ""
        self.appendNewline = snippet?.appendNewline ?? true
        self.profileId = snippet?.profileId
    }

    public func loadAvailableProfiles() {
        availableProfiles = (try? db.fetchAllProfiles()) ?? []
    }

    /// 保存に成功すれば`true`を返す。
    public func save() -> Bool {
        errorMessage = nil
        guard !label.trimmingCharacters(in: .whitespaces).isEmpty else {
            errorMessage = "ラベルを入力してください"
            return false
        }
        guard !command.trimmingCharacters(in: .whitespaces).isEmpty else {
            errorMessage = "コマンドを入力してください"
            return false
        }

        var snippet = Snippet(
            id: existingId,
            label: label.trimmingCharacters(in: .whitespaces),
            command: command,
            sortOrder: existingSortOrder,
            profileId: profileId,
            appendNewline: appendNewline
        )
        do {
            if existingId != nil {
                try db.update(snippet: snippet)
            } else {
                try db.insert(snippet: &snippet)
            }
            return true
        } catch {
            errorMessage = "保存に失敗しました: \(error)"
            return false
        }
    }
}

public struct SnippetEditView: View {
    @StateObject private var model: SnippetEditModel
    private let onSave: () -> Void
    private let onCancel: () -> Void
    private let isNew: Bool

    public init(
        snippet: Snippet?,
        onSave: @escaping () -> Void,
        onCancel: @escaping () -> Void
    ) {
        _model = StateObject(wrappedValue: SnippetEditModel(snippet: snippet))
        self.onSave = onSave
        self.onCancel = onCancel
        self.isNew = snippet == nil
    }

    public var body: some View {
        Form {
            Section("定型コマンド") {
                TextField("ラベル", text: $model.label)
                    .accessibilityIdentifier("snippetLabelField")
                TextField("コマンド(複数行可)", text: $model.command, axis: .vertical)
                    .lineLimit(4...10)
                    .accessibilityIdentifier("snippetCommandField")
                Text("注意: パスワードなどの機密情報をここに平文で書くと、保護されずデータベースに残ります。")
                    .font(.caption)
                    .foregroundStyle(.red)
            }

            Section {
                Toggle("末尾でEnterする", isOn: $model.appendNewline)
                    .accessibilityIdentifier("appendNewlineToggle")
            }

            Section("適用範囲") {
                Picker("プロファイル", selection: $model.profileId) {
                    Text("全プロファイル共通").tag(Int64?.none)
                    ForEach(model.availableProfiles, id: \.id) { profile in
                        Text(profile.displayName).tag(profile.id)
                    }
                }
                .accessibilityIdentifier("snippetProfilePicker")
            }

            if let error = model.errorMessage {
                Section {
                    Text(error)
                        .foregroundStyle(.red)
                        .accessibilityIdentifier("snippetEditError")
                }
            }
        }
        .navigationTitle(isNew ? "定型コマンド追加" : "定型コマンド編集")
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                Button("キャンセル", action: onCancel)
            }
            ToolbarItem(placement: .confirmationAction) {
                Button("保存") {
                    if model.save() { onSave() }
                }
                .accessibilityIdentifier("saveSnippetButton")
            }
        }
        .onAppear { model.loadAvailableProfiles() }
    }
}
