import SwiftUI
import IsekaiTerminalCoreLogic

/// Android版`KeySequenceEditScreen.kt`/`KeySequenceEditViewModel.kt`の移植。
/// 単一テキスト欄の[SnippetEditModel]と異なり、Ctrlチョード/テキスト/特殊キーを
/// 積み木式に組み立てる[steps]配列を保持する。
@MainActor
public final class KeySequenceEditModel: ObservableObject {
    @Published public var label: String
    @Published public var steps: [KeyStep]
    @Published public var profileId: Int64?
    @Published public var availableProfiles: [ConnectionProfile] = []
    @Published public var errorMessage: String?

    private let db: ProfileDatabase
    private let existingId: Int64?
    private let existingSortOrder: Int

    public init(keySequence: KeySequence?, db: ProfileDatabase = AppServices.shared.db) {
        self.db = db
        self.existingId = keySequence?.id
        self.existingSortOrder = keySequence?.sortOrder ?? 0
        self.label = keySequence?.label ?? ""
        self.steps = keySequence?.steps ?? []
        self.profileId = keySequence?.profileId
    }

    public func loadAvailableProfiles() {
        availableProfiles = (try? db.fetchAllProfiles()) ?? []
    }

    public func addCtrlChar(_ char: Character) {
        steps.append(.ctrlChar(char))
    }

    public func addText(_ text: String) {
        guard !text.isEmpty else { return }
        steps.append(.text(text))
    }

    public func addSpecial(_ key: TerminalKeyMapper.SpecialKey) {
        steps.append(.special(key))
    }

    public func removeStep(at index: Int) {
        guard steps.indices.contains(index) else { return }
        steps.remove(at: index)
    }

    /// steps.isEmpty だけでは、Ctrl+1 のような変換不能な文字だけのstepでも保存できてしまい
    /// 送信時に無音no-opになる(Android版と同じcodexレビュー指摘を踏襲)。実際にバイト列が
    /// 出力されることまで確認する。
    public var canSave: Bool {
        !label.trimmingCharacters(in: .whitespaces).isEmpty && !KeySequenceCommands.toBytes(steps).isEmpty
    }

    /// 保存に成功すれば`true`を返す。
    public func save() -> Bool {
        errorMessage = nil
        guard !label.trimmingCharacters(in: .whitespaces).isEmpty else {
            errorMessage = "ラベルを入力してください"
            return false
        }
        guard !KeySequenceCommands.toBytes(steps).isEmpty else {
            errorMessage = "有効な打鍵列を1つ以上追加してください"
            return false
        }

        var keySequence = KeySequence(
            id: existingId,
            label: label.trimmingCharacters(in: .whitespaces),
            stepsJson: KeyStepJSON.encode(steps),
            sortOrder: existingSortOrder,
            profileId: profileId
        )
        do {
            if existingId != nil {
                try db.update(keySequence: keySequence)
            } else {
                try db.insert(keySequence: &keySequence)
            }
            return true
        } catch {
            errorMessage = "保存に失敗しました: \(error)"
            return false
        }
    }
}

public struct KeySequenceEditView: View {
    @StateObject private var model: KeySequenceEditModel
    private let onSave: () -> Void
    private let onCancel: () -> Void
    private let isNew: Bool

    @State private var ctrlCharInput = ""
    @State private var textStepInput = ""
    @State private var selectedSpecialKey: TerminalKeyMapper.SpecialKey = SpecialKeyChoices.all[0].key

    public init(
        keySequence: KeySequence?,
        onSave: @escaping () -> Void,
        onCancel: @escaping () -> Void
    ) {
        _model = StateObject(wrappedValue: KeySequenceEditModel(keySequence: keySequence))
        self.onSave = onSave
        self.onCancel = onCancel
        self.isNew = keySequence == nil
    }

    public var body: some View {
        Form {
            Section("打鍵列") {
                TextField("ラベル（例: tmux 新規ウィンドウ）", text: $model.label)
                    .accessibilityIdentifier("keySequenceLabelField")
                Text("注意: パスワードなどの機密情報をテキストステップに書くと、保護されずデータベースに残ります。")
                    .font(.caption)
                    .foregroundStyle(.red)
            }

            Section("ステップ") {
                if model.steps.isEmpty {
                    Text("まだステップがありません。下から追加してください。")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(Array(model.steps.enumerated()), id: \.offset) { index, step in
                        HStack {
                            Text(step.shortLabel)
                                .font(.system(.body, design: .monospaced))
                            Spacer()
                            Button("削除", role: .destructive) { model.removeStep(at: index) }
                        }
                    }
                }
            }

            Section("ステップを追加") {
                HStack {
                    TextField("Ctrl+ (1文字)", text: $ctrlCharInput)
                        .accessibilityIdentifier("ctrlCharInputField")
                    Button("Ctrlを追加") {
                        if let c = ctrlCharInput.first {
                            model.addCtrlChar(c)
                            ctrlCharInput = ""
                        }
                    }
                    // 変換不能な文字(数字・日本語等)は追加できないようにする(codexレビュー指摘:
                    // 追加できてしまうと、全体のcanSaveは通らなくても他のステップと組み合わせた
                    // 「部分的に無効なstepを含む打鍵列」が作れてしまい、送信時に無視される)。
                    .disabled(ctrlCharInput.first.flatMap(TerminalKeyMapper.controlByte(for:)) == nil)
                }
                HStack {
                    TextField("テキスト", text: $textStepInput)
                        .accessibilityIdentifier("textStepInputField")
                    Button("追加") {
                        model.addText(textStepInput)
                        textStepInput = ""
                    }
                    .disabled(textStepInput.isEmpty)
                }
                HStack {
                    Picker("特殊キー", selection: $selectedSpecialKey) {
                        ForEach(SpecialKeyChoices.all, id: \.label) { choice in
                            Text(choice.label).tag(choice.key)
                        }
                    }
                    Button("追加") { model.addSpecial(selectedSpecialKey) }
                }
                // Enterは[TerminalKeyMapper.SpecialKey]に相当するcaseが無い(物理/ソフトの
                // Enterキーは通常のテキスト入力経路で`\r`として扱われるため)。ここでは
                // `KeyStep.text("\r")`として追加する専用ボタンを用意する。
                Button("Enterを追加") { model.addText("\r") }
            }

            Section("適用範囲") {
                Picker("プロファイル", selection: $model.profileId) {
                    Text("全プロファイル共通").tag(Int64?.none)
                    ForEach(model.availableProfiles, id: \.id) { profile in
                        Text(profile.displayName).tag(profile.id)
                    }
                }
                .accessibilityIdentifier("keySequenceProfilePicker")
            }

            if let error = model.errorMessage {
                Section {
                    Text(error)
                        .foregroundStyle(.red)
                        .accessibilityIdentifier("keySequenceEditError")
                }
            }
        }
        .navigationTitle(isNew ? "打鍵列追加" : "打鍵列編集")
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                Button("キャンセル", action: onCancel)
            }
            ToolbarItem(placement: .confirmationAction) {
                Button("保存") {
                    if model.save() { onSave() }
                }
                .disabled(!model.canSave)
                .accessibilityIdentifier("saveKeySequenceButton")
            }
        }
        .onAppear { model.loadAvailableProfiles() }
    }
}
