import SwiftUI

/// Phase 1C(#25): trzszファイル転送シート。Android版`TrzszTransferSheet.kt`
/// (`ModalBottomSheet`)と対称の3状態表示(待機/進行中/完了)。
struct TrzszTransferSheet: View {
    let state: TrzszUiState
    /// ダウンロード成功時のみ非nil。非nilなら完了画面に「保存」ボタンを出す。
    let completedDownloadURL: URL?
    let onStartUpload: () -> Void
    let onStartDownload: () -> Void
    let onCancel: () -> Void
    let onSave: () -> Void
    let onDismiss: () -> Void

    var body: some View {
        VStack(spacing: 16) {
            switch state {
            case .waitingUser(_, let mode, let suggestedName, let expectedSize):
                waitingUserView(mode: mode, suggestedName: suggestedName, expectedSize: expectedSize)
            case .inProgress(_, _, let fileName, let transferred, let total):
                inProgressView(fileName: fileName, transferred: transferred, total: total)
            case .done(_, let success, let message):
                doneView(success: success, message: message)
            }
        }
        .padding()
        .accessibilityIdentifier("trzszTransferSheet")
    }

    @ViewBuilder
    private func waitingUserView(mode: String, suggestedName: String?, expectedSize: UInt64?) -> some View {
        if mode == "upload" {
            Text("ファイルを送信").font(.headline)
            Button("ファイルを選択") { onStartUpload() }
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("trzszSelectFileButton")
        } else {
            Text("ファイルを受信").font(.headline)
            if let suggestedName {
                Text(suggestedName).foregroundStyle(.secondary)
            }
            if let expectedSize {
                Text(Self.byteCountFormatter.string(fromByteCount: Int64(expectedSize)))
                    .foregroundStyle(.secondary)
            }
            Button("受信開始") { onStartDownload() }
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("trzszStartDownloadButton")
        }
        Button("キャンセル", role: .cancel) { onCancel() }
            .accessibilityIdentifier("trzszCancelButton")
    }

    @ViewBuilder
    private func inProgressView(fileName: String?, transferred: UInt64, total: UInt64?) -> some View {
        if let fileName {
            Text(fileName).font(.headline)
        }
        if let total {
            ProgressView(value: Double(transferred), total: Double(total))
        } else {
            ProgressView()
        }
        Text(progressLabel(transferred: transferred, total: total))
            .foregroundStyle(.secondary)
        Button("キャンセル", role: .cancel) { onCancel() }
            .accessibilityIdentifier("trzszCancelButton")
    }

    @ViewBuilder
    private func doneView(success: Bool, message: String?) -> some View {
        Text(success ? "転送完了" : "転送失敗")
            .font(.headline)
            .foregroundStyle(success ? .primary : .red)
        if let message {
            Text(message).foregroundStyle(.secondary)
        }
        if completedDownloadURL != nil {
            Button("保存") { onSave() }
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("trzszSaveButton")
        }
        Button("閉じる") { onDismiss() }
            .accessibilityIdentifier("trzszDismissButton")
    }

    private func progressLabel(transferred: UInt64, total: UInt64?) -> String {
        let transferredText = Self.byteCountFormatter.string(fromByteCount: Int64(transferred))
        guard let total else { return transferredText }
        return "\(transferredText) / \(Self.byteCountFormatter.string(fromByteCount: Int64(total)))"
    }

    private static let byteCountFormatter: ByteCountFormatter = {
        let formatter = ByteCountFormatter()
        formatter.countStyle = .file
        return formatter
    }()
}
