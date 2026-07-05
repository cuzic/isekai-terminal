import SwiftUI
import TsshCoreLogic

/// Phase 1G-1(#53): Android版`SnippetListScreen.kt`/`SnippetListViewModel.kt`の移植。
@MainActor
public final class SnippetListModel: ObservableObject {
    @Published public private(set) var snippets: [Snippet] = []
    @Published public var deleteTarget: Snippet?

    private let db: ProfileDatabase

    public init(db: ProfileDatabase = AppServices.shared.db) {
        self.db = db
    }

    public func load() {
        snippets = (try? db.fetchAllSnippets()) ?? []
    }

    public func requestDelete(_ snippet: Snippet) { deleteTarget = snippet }
    public func dismissDelete() { deleteTarget = nil }

    public func confirmDelete(_ snippet: Snippet) {
        deleteTarget = nil
        guard let id = snippet.id else { return }
        try? db.deleteSnippet(id: id)
        load()
    }
}

public struct SnippetListView: View {
    @StateObject private var model: SnippetListModel
    private let onAddSnippet: () -> Void
    private let onEditSnippet: (Snippet) -> Void

    public init(
        model: SnippetListModel,
        onAddSnippet: @escaping () -> Void,
        onEditSnippet: @escaping (Snippet) -> Void
    ) {
        _model = StateObject(wrappedValue: model)
        self.onAddSnippet = onAddSnippet
        self.onEditSnippet = onEditSnippet
    }

    public var body: some View {
        List {
            if model.snippets.isEmpty {
                Text("「＋」をタップして定型コマンドを追加")
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("snippetListEmptyHint")
            }
            ForEach(model.snippets, id: \.id) { snippet in
                SnippetRow(snippet: snippet)
                    .contentShape(Rectangle())
                    .onTapGesture { onEditSnippet(snippet) }
                    .accessibilityIdentifier("snippetRow_\(snippet.id.map(String.init) ?? "new")")
                    .swipeActions {
                        Button("削除", role: .destructive) { model.requestDelete(snippet) }
                        Button("編集") { onEditSnippet(snippet) }.tint(.blue)
                    }
            }
        }
        .accessibilityIdentifier("snippetList")
        .navigationTitle("定型コマンド")
        .toolbar {
            ToolbarItem(placement: .navigationBarTrailing) {
                Button(action: onAddSnippet) {
                    Image(systemName: "plus")
                }
                .accessibilityIdentifier("addSnippetButton")
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

private struct SnippetRow: View {
    let snippet: Snippet

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(snippet.label)
                .font(.headline)
            Text(snippet.command.split(separator: "\n").first.map(String.init) ?? "")
                .font(.system(.subheadline, design: .monospaced))
                .foregroundStyle(.secondary)
                .lineLimit(1)
            Text(snippet.profileId == nil ? "全プロファイル共通" : "特定プロファイル専用")
                .font(.caption)
                .foregroundStyle(.tint)
        }
        .padding(.vertical, 2)
    }
}
