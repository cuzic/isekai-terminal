import Foundation
#if canImport(UIKit)
import UIKit
#endif
import IsekaiTerminalCoreLogic

/// リモート(OSC 52 / tmux迂回チャンネル)とデバイスのクリップボード同期の設定。
/// Android版`TerminalTabsViewModel`が`SharedPreferences`キー
/// `PREF_KEY_ALLOW_REMOTE_CLIPBOARD_WRITE`/`PREF_KEY_ALLOW_REMOTE_CLIPBOARD_PULL`で
/// 管理しているのと同じ「既定オプトアウト」方針を`UserDefaults`で踏襲する
/// (`.claude/rules/rust-ssot.md`の対象外——セッション/プロトコル状態ではなく単なるUI設定のため)。
enum RemoteClipboardSettings {
    static let allowRemoteClipboardWriteKey = AppSettingsKeys.allowRemoteClipboardWrite
    static let allowRemoteClipboardPullKey = AppSettingsKeys.allowRemoteClipboardPull

    static func isWriteAllowed(defaults: UserDefaults = .standard) -> Bool {
        defaults.bool(forKey: allowRemoteClipboardWriteKey)
    }

    static func isPullAllowed(defaults: UserDefaults = .standard) -> Bool {
        defaults.bool(forKey: allowRemoteClipboardPullKey)
    }
}

/// `RemoteClipboardBridge.readOnMain`がmainで得た値を呼び出し元スレッドへ渡すための箱
/// (タイムアウト後にmain側が書き込んでも競合しないようロックで保護する)。
private final class MainThreadResultBox<T>: @unchecked Sendable {
    let lock = NSLock()
    var value: T?
}

/// `TerminalSessionController.onClipboardWrite`/`onClipboardPullRequest`(`OrchestratorCallback`)の
/// 実処理。Android版`RemoteClipboardPolicy`/`RemoteClipboardImagePolicy`のiOS版に相当する。
/// Android版と異なりiOSの`UIPasteboard`は画像を直接扱えるため、`FileProvider`相当の
/// 一時ファイル経由URI発行は不要(`.image`/`.string`への読み書きだけで完結する)。
///
/// スレッド: 呼び出し元の`onClipboardWrite`/`onClipboardPullRequest`はRustのコールバック
/// スレッドから呼ばれるが、`UIPasteboard`(UIKit)への実アクセスはmain threadで行う
/// (2026-09-29レビューIOS-L3)。書き込みはmainへ非同期に投げるだけ。読み出しは同期的に
/// 値を返す必要があるため、mainで読んだ結果をセマフォで待つ——ただしmainが(Rust呼び出し
/// 等で)塞がっていてもRust側を永久にブロック/デッドロックさせないよう、
/// `pullTimeout`を超えたら「取得不可」(nil)として返す。
enum RemoteClipboardBridge {
    static let pullTimeout: DispatchTimeInterval = .seconds(5)

    static func write(_ payload: ClipboardPayload, defaults: UserDefaults = .standard) {
        guard RemoteClipboardSettings.isWriteAllowed(defaults: defaults) else { return }
        #if canImport(UIKit)
        performOnMain {
            switch payload.mime {
            case .imagePng:
                guard let image = UIImage(data: payload.data) else { return }
                UIPasteboard.general.image = image
            case .textPlain, .textHtml:
                guard let text = String(data: payload.data, encoding: .utf8) else { return }
                UIPasteboard.general.string = text
            }
        }
        #endif
    }

    static func pull(defaults: UserDefaults = .standard) -> ClipboardPayload? {
        guard RemoteClipboardSettings.isPullAllowed(defaults: defaults) else { return nil }
        #if canImport(UIKit)
        return readOnMain(timeout: pullTimeout) {
            let pasteboard = UIPasteboard.general
            if let image = pasteboard.image, let png = image.pngData() {
                return ClipboardPayload(mime: .imagePng, data: png)
            }
            if let text = pasteboard.string, !text.isEmpty {
                return ClipboardPayload(mime: .textPlain, data: Data(text.utf8))
            }
            return nil
        }
        #else
        return nil
        #endif
    }

    /// main threadなら即座に、それ以外ならmainへ非同期に`body`を実行する。
    static func performOnMain(_ body: @escaping () -> Void) {
        if Thread.isMainThread {
            body()
        } else {
            DispatchQueue.main.async(execute: body)
        }
    }

    /// main threadで`body`を評価した結果を返す。main以外から呼ばれた場合は最大`timeout`だけ
    /// 待ち、間に合わなければnilを返す(呼び出し元のスレッドを永久にブロックしない)。
    static func readOnMain<T>(timeout: DispatchTimeInterval, _ body: @escaping () -> T?) -> T? {
        if Thread.isMainThread {
            return body()
        }
        let box = MainThreadResultBox<T>()
        let semaphore = DispatchSemaphore(value: 0)
        DispatchQueue.main.async {
            let value = body()
            box.lock.lock()
            box.value = value
            box.lock.unlock()
            semaphore.signal()
        }
        guard semaphore.wait(timeout: .now() + timeout) == .success else { return nil }
        box.lock.lock()
        defer { box.lock.unlock() }
        return box.value
    }
}
