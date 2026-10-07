import Foundation

/// Phase 1C(#25): trzszファイル転送のUI状態。Android版`TrzszUiState`(sealed class、
/// `WaitingUser`/`InProgress`/`Done`)と対称。
public enum TrzszUiState: Equatable {
    /// リモートから転送要求が来た直後、ユーザーの操作待ち。`mode`は`"upload"`/
    /// `"download"`/`"dir"`(Rust側`TrzszMode`由来の文字列)。
    case waitingUser(transferId: String, mode: String, suggestedName: String?, expectedSize: UInt64?)
    /// 転送実行中。`fileName`はアップロードなら選択したファイル名、ダウンロードなら
    /// `suggestedName`をそのまま引き継ぐ。
    case inProgress(transferId: String, mode: String, fileName: String?, transferred: UInt64, total: UInt64?)
    /// 転送完了(成功/失敗いずれも)。
    case done(transferId: String, success: Bool, message: String?)
}

extension TerminalSessionController {
    /// アップロード時に使う読み出しチャンクサイズ。Android版
    /// `TerminalTabsViewModel.trzszStartUpload`と同じ64KB。
    static let trzszChunkSize = 64 * 1024

    /// 「次のチャンクを1つ先読みして`isLast`を判定する」読み出しループの中核ロジック。
    /// `readNext`はEOFで空`Data`を返す規約(`FileHandle.readData(ofLength:)`と同じ)。
    /// 実アップロード(`trzszStartUpload`)とテストの両方から同じ関数を使うことで、
    /// チャンク境界(特に0バイトファイル・ちょうどchunkSize境界)のロジックを実ファイル
    /// I/Oなしで検証できる。
    ///
    /// `readNext`は読み出しエラーをthrowしてよく、その場合は(isLast=trueのチャンクを
    /// 送らずに)そのままエラーを呼び出し元へ返す。実アップロードでは
    /// `FileHandle.read(upToCount:)`(throws)を使う: `readData(ofLength:)`はI/Oエラー時に
    /// Objective-C例外を投げ、Swiftでは捕捉できずクラッシュするため(iCloudから退避済みの
    /// ファイル等、2026-09-29レビューIOS-I8)。
    static func trzszSendChunked(readNext: () throws -> Data, send: (Data, Bool) -> Void) rethrows {
        var chunk = try readNext()
        while true {
            let next = try readNext()
            let isLast = next.isEmpty
            send(chunk, isLast)
            if isLast { break }
            chunk = next
        }
    }
}
