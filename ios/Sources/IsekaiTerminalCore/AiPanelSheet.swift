import SwiftUI
import IsekaiTerminalCoreLogic

/// Y-P1(#2): `AI_INTEGRATION_DESIGN.md` §6にあるリモートAPC経由の構造化パネル
/// (`presentDocument`/`presentForm`)を表示するシート。Android版`AiPanelDialog.kt`の
/// `AiPanelSheet`と対称(`ADR_IOS_PARITY_IMPLEMENTATION.md` §3.2)。
///
/// **信頼境界**: `panel`の内容(title/markdown/fields)はリモートの任意プロセスが
/// 偽造できるPTY上のin-bandデータであり、このシートはそれを**表示専用テキストとして
/// しか扱わない**——自動実行・シェルコマンド化・クリップボード自動書き込み等の副作用を
/// 一切引き起こさない(`ai_panel.rs`のモジュールdoc参照)。冒頭に「リモートから受信」
/// である旨を常に明示する。Markdownは構文解釈せず生テキストとして表示する(MVPスコープ、
/// SwiftUIが`LocalizedStringKey`経路でMarkdownを解釈しないよう`Text(verbatim:)`を使う)。
///
/// `presentForm`の送信結果は`onSubmit`経由でPTYへの通常のテキスト入力として返るだけで
/// (`TerminalSessionController.submitAiPanelForm`参照)、専用の実行チャネルは無い。
struct AiPanelSheet: View {
    let panel: AiPanelUiState
    let onSubmit: ([String: String]) -> Void
    let onDismiss: () -> Void

    @State private var fieldValues: [String: String] = [:]

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    Text("📡 リモートから受信したパネル")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Text(panel.title.isEmpty ? "(無題)" : panel.title)
                        .font(.headline)

                    switch panel.kind {
                    case .document:
                        Text(verbatim: panel.markdown)
                    case .form:
                        ForEach(panel.fields, id: \.id) { field in
                            AiPanelFormFieldView(
                                field: field,
                                value: fieldValues[field.id] ?? "",
                                onValueChange: { fieldValues[field.id] = $0 }
                            )
                        }
                    case .none:
                        EmptyView()
                    }
                }
                .padding()
            }
            .navigationTitle("パネル")
            .navigationBarTitleDisplayMode(.inline)
            .accessibilityIdentifier("aiPanelSheet")
            .toolbar {
                if panel.kind == .form {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("キャンセル", action: onDismiss)
                    }
                    ToolbarItem(placement: .confirmationAction) {
                        Button("送信") { onSubmit(fieldValues) }
                            .accessibilityIdentifier("aiPanelSubmitButton")
                    }
                } else {
                    ToolbarItem(placement: .confirmationAction) {
                        Button("閉じる", action: onDismiss)
                    }
                }
            }
        }
    }
}

private struct AiPanelFormFieldView: View {
    let field: PanelField
    let value: String
    let onValueChange: (String) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(field.label)
                .font(.subheadline.weight(.medium))
            switch field.kind {
            case .text:
                TextField(field.label, text: Binding(get: { value }, set: onValueChange))
                    .textFieldStyle(.roundedBorder)
                    .accessibilityIdentifier("aiPanelField_\(field.id)")
            case .choice:
                // Y-P0の教訓(トグル/`Switch`のアクセシビリティ値がCIで安定しなかった)を
                // 踏まえ、選択状態はラベルテキストへ埋め込むButton方式にする
                // (`SettingsView.swift`と同じ、実績のあるパターン)。
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(field.options, id: \.self) { option in
                        Button {
                            onValueChange(option)
                        } label: {
                            Text(value == option ? "\(option) ✓" : option)
                        }
                        .accessibilityIdentifier("aiPanelChoice_\(field.id)_\(option)")
                    }
                }
            }
        }
    }
}
