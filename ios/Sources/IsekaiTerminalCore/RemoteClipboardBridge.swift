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

/// `TerminalSessionController.onClipboardWrite`/`onClipboardPullRequest`(`SessionCallback`)の
/// 実処理。Android版`RemoteClipboardPolicy`/`RemoteClipboardImagePolicy`のiOS版に相当する。
/// Android版と異なりiOSの`UIPasteboard`は画像を直接扱えるため、`FileProvider`相当の
/// 一時ファイル経由URI発行は不要(`.image`/`.string`への読み書きだけで完結する)。
enum RemoteClipboardBridge {
    static func write(_ payload: ClipboardPayload, defaults: UserDefaults = .standard) {
        guard RemoteClipboardSettings.isWriteAllowed(defaults: defaults) else { return }
        #if canImport(UIKit)
        switch payload.mime {
        case .imagePng:
            guard let image = UIImage(data: payload.data) else { return }
            UIPasteboard.general.image = image
        case .textPlain, .textHtml:
            guard let text = String(data: payload.data, encoding: .utf8) else { return }
            UIPasteboard.general.string = text
        }
        #endif
    }

    static func pull(defaults: UserDefaults = .standard) -> ClipboardPayload? {
        guard RemoteClipboardSettings.isPullAllowed(defaults: defaults) else { return nil }
        #if canImport(UIKit)
        let pasteboard = UIPasteboard.general
        if let image = pasteboard.image, let png = image.pngData() {
            return ClipboardPayload(mime: .imagePng, data: png)
        }
        if let text = pasteboard.string, !text.isEmpty {
            return ClipboardPayload(mime: .textPlain, data: Data(text.utf8))
        }
        return nil
        #else
        return nil
        #endif
    }
}
