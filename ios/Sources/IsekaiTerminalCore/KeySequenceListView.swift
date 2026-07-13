import SwiftUI
import IsekaiTerminalCoreLogic

/// Android版`KeySequenceListScreen.kt`/`KeySequenceListViewModel.kt`の移植。
@MainActor
public final class KeySequenceListModel: ObservableObject {
    @Published public private(set) var keySequences: [KeySequence] = []
    @Published public var deleteTarget: KeySequence?

    // ── 打鍵列セット(パック) ──────────────────────────────
    // MVPではグローバル有効化(profileId=nil)のみをこの一覧画面から操作できるようにする
    // (Android版KeySequenceListViewModelと同じ判断)。
    public let packs: [KeySequencePack] = KeySequencePacks.all
    @Published public private(set) var globalInstallations: [String: KeySequencePackInstallation] = [:]

    private let db: ProfileDatabase

    public init(db: ProfileDatabase = AppServices.shared.db) {
        self.db = db
    }

    public func load() {
        keySequences = (try? db.fetchAllKeySequences()) ?? []
        loadPackInstallations()
    }

    public func loadPackInstallations() {
        var result: [String: KeySequencePackInstallation] = [:]
        for pack in packs {
            if let installation = try? db.fetchGlobalPackInstallation(packId: pack.id) {
                result[pack.id] = installation
            }
        }
        globalInstallations = result
    }

    public func activatePack(_ pack: KeySequencePack, prefixChar: Character) {
        try? db.installPack(packId: pack.id, version: pack.version, paramValues: ["prefix": .ctrlChar(prefixChar)], profileId: nil)
        loadPackInstallations()
    }

    public func deactivatePack(_ installation: KeySequencePackInstallation) {
        guard let id = installation.id else { return }
        try? db.deletePackInstallation(id: id)
        loadPackInstallations()
    }

    public func requestDelete(_ keySequence: KeySequence) { deleteTarget = keySequence }
    public func dismissDelete() { deleteTarget = nil }

    public func confirmDelete(_ keySequence: KeySequence) {
        deleteTarget = nil
        guard let id = keySequence.id else { return }
        try? db.deleteKeySequence(id: id)
        load()
    }
}

public struct KeySequenceListView: View {
    @StateObject private var model: KeySequenceListModel
    private let onAddKeySequence: () -> Void
    private let onEditKeySequence: (KeySequence) -> Void

    public init(
        model: KeySequenceListModel,
        onAddKeySequence: @escaping () -> Void,
        onEditKeySequence: @escaping (KeySequence) -> Void
    ) {
        _model = StateObject(wrappedValue: model)
        self.onAddKeySequence = onAddKeySequence
        self.onEditKeySequence = onEditKeySequence
    }

    public var body: some View {
        List {
            if !model.packs.isEmpty {
                Section("パック") {
                    ForEach(model.packs, id: \.id) { pack in
                        KeySequencePackRow(
                            pack: pack,
                            installation: model.globalInstallations[pack.id],
                            onActivate: { prefixChar in model.activatePack(pack, prefixChar: prefixChar) },
                            onDeactivate: { installation in model.deactivatePack(installation) }
                        )
                    }
                }
            }
            Section("打鍵列") {
                if model.keySequences.isEmpty {
                    Text("「＋」をタップして打鍵列を追加")
                        .foregroundStyle(.secondary)
                        .accessibilityIdentifier("keySequenceListEmptyHint")
                }
                ForEach(model.keySequences, id: \.id) { keySequence in
                    KeySequenceRow(keySequence: keySequence)
                        .contentShape(Rectangle())
                        .onTapGesture { onEditKeySequence(keySequence) }
                        .accessibilityIdentifier("keySequenceRow_\(keySequence.id.map(String.init) ?? "new")")
                        .swipeActions {
                            Button("削除", role: .destructive) { model.requestDelete(keySequence) }
                            Button("編集") { onEditKeySequence(keySequence) }.tint(.blue)
                        }
                }
            }
        }
        .accessibilityIdentifier("keySequenceList")
        .navigationTitle("打鍵列")
        .toolbar {
            ToolbarItem(placement: .navigationBarTrailing) {
                Button(action: onAddKeySequence) {
                    Image(systemName: "plus")
                }
                .accessibilityIdentifier("addKeySequenceButton")
            }
        }
        .onAppear { model.load() }
        .alert(
            "削除確認",
            isPresented: Binding(
                get: { model.deleteTarget != nil },
                set: { if !$0 { model.dismissDelete() } }
            )
        ) {
            Button("キャンセル", role: .cancel) { model.dismissDelete() }
            Button("削除", role: .destructive) {
                if let target = model.deleteTarget { model.confirmDelete(target) }
            }
        } message: {
            Text("「\(model.deleteTarget?.label ?? "")」を削除しますか？")
        }
    }
}

private struct KeySequenceRow: View {
    let keySequence: KeySequence

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(keySequence.label)
                .font(.headline)
            Text(keySequence.steps.previewText)
                .font(.system(.subheadline, design: .monospaced))
                .foregroundStyle(.secondary)
                .lineLimit(1)
            Text(keySequence.profileId == nil ? "全プロファイル共通" : "特定プロファイル専用")
                .font(.caption)
                .foregroundStyle(.tint)
        }
        .padding(.vertical, 2)
    }
}

/// 打鍵列セット(パック)の有効化状態行。MVPではグローバル有効化(profileId=nil)のみを
/// この画面から操作できる(Android版`KeySequencePackCard`と同じ判断)。prefixキーの入力は
/// [KeySequenceEditView]のCtrlチョード追加欄と同じ「1文字入力」方式。
private struct KeySequencePackRow: View {
    let pack: KeySequencePack
    let installation: KeySequencePackInstallation?
    let onActivate: (Character) -> Void
    let onDeactivate: (KeySequencePackInstallation) -> Void

    @State private var prefixInput: String = ""

    private var currentPrefixChar: Character? {
        if case .ctrlChar(let c)? = installation?.paramValues["prefix"] { return c }
        if case .ctrlChar(let c)? = pack.params.first(where: { $0.name == "prefix" })?.defaultStep { return c }
        return nil
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(pack.name).font(.headline)
                Spacer()
                // 三項演算子で.secondaryと.tintを直接返すと、HierarchicalShapeStyleと
                // TintShapeStyleの型不一致でSwiftUIの型推論が通らない可能性が高い
                // (codexレビュー指摘)。同一型(Color)へ揃える。
                if installation == nil {
                    Text("未有効化").font(.caption).foregroundStyle(Color.secondary)
                } else {
                    Text("有効").font(.caption).foregroundStyle(Color.accentColor)
                }
            }
            Text(pack.sequences.map(\.label).joined(separator: " / "))
                .font(.caption)
                .foregroundStyle(.secondary)
            HStack {
                TextField("Ctrl+ (1文字)", text: $prefixInput)
                    .textFieldStyle(.roundedBorder)
                    .frame(width: 120)
                Button(installation == nil ? "有効化" : "更新") {
                    if let c = prefixInput.first { onActivate(c) }
                }
                // 変換不能な文字(数字・日本語等)をprefixとして保存すると、tmuxパックの
                // prefixチョードが無音になり後続ステップだけ送信される誤動作になる
                // (codexレビュー指摘)。TerminalKeyMapper.controlByteで事前に検証する。
                .disabled(prefixInput.first.flatMap(TerminalKeyMapper.controlByte(for:)) == nil)
                if let installation {
                    Button("無効化", role: .destructive) { onDeactivate(installation) }
                }
            }
        }
        .padding(.vertical, 4)
        .onAppear { prefixInput = currentPrefixChar.map(String.init) ?? "" }
        // iOS 16対応: 2引数版onChange(iOS 17+)ではなく既存コード(TerminalTabsHostView.swift)と
        // 同じ1引数版を使う。
        .onChange(of: installation) { newValue in
            if case .ctrlChar(let c)? = newValue?.paramValues["prefix"] {
                prefixInput = String(c)
            }
        }
    }
}
