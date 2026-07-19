import Foundation

/// Phase 1B: ターミナル特殊キー操作(#11c/d)のうち、実機を必要としない
/// 「キー→制御シーケンス変換」ロジックの部分。キーボードアクセサリバーの
/// 見た目・レイアウトや選択/コピー/ペーストのUI・Dynamic Typeとは独立した
/// フォントサイズ設定UIは実機/シミュレータでの目視確認が必要なため、
/// この実装のスコープには含めない(PLAN.md「Phase Y」節参照)。
///
/// 実際の変換ロジックはRust側(`terminal_ctrl_byte`/`terminal_special_key_bytes`)に
/// 統合済み(Android版`TerminalKeyEncoder.kt`とのAndroid/iOS共通化、rust-core側が
/// SSOT)。この型は既存のSwift APIをそのまま維持する薄いラッパー。
/// `applicationCursorMode`(DECCKM)を意識しない、常にCSI形式を返す従来のSwift API
/// はそのまま残しつつ、内部実装だけをRustへ委譲している。
public enum TerminalKeyMapper {
    /// Ctrl+<英字>を対応する制御バイト(0x01〜0x1A)に変換する。
    /// 大文字・小文字どちらの入力でも同じ結果になる(実際のCtrlキーの挙動に合わせる)。
    public static func controlByte(for letter: Character) -> UInt8? {
        guard let ascii = letter.asciiValue else { return nil }
        return terminalCtrlByte(codePoint: UInt32(ascii))
    }

    /// `Hashable`は打鍵列(KeySequence)編集UIの`Picker`選択値として使うために追加
    /// (`functionKey(Int)`の連想値もHashableなので自動合成される)。
    public enum SpecialKey: Equatable, Hashable {
        case escape
        case tab
        case backspace
        case delete
        case arrowUp, arrowDown, arrowLeft, arrowRight
        case home
        case end
        case pageUp
        case pageDown
        case functionKey(Int) // F1〜F12
    }

    /// 特殊キーに対応する、ターミナルへ送信するバイト列(xterm互換のANSI
    /// エスケープシーケンス)を返す。未対応のfunction key番号は空配列を返す。
    /// `applicationCursorMode`(DECCKM)を意識しない、常にCSI形式を返す従来のAPI。
    public static func bytes(for key: SpecialKey) -> [UInt8] {
        bytes(for: key, applicationCursorMode: false)
    }

    /// 打鍵列(KeySequence)機能向け: `applicationCursorMode`(DECCKM)を明示的に指定できる版。
    /// 矢印キー等はtmux/vim等でDECCKMがオンの場合SS3形式になる(Android版
    /// `TerminalKeyEncoder.specialKeyBytes(keyCode, applicationCursorMode)`と同じ挙動)。
    ///
    /// `modifiers`(Shift/Alt/Ctrl/Meta)はRust側の`terminal_special_key_bytes`(#29)へ
    /// そのまま委譲する(ハードウェアキーボード接続時のUI配線本体は#63)。UniFFIが生成した
    /// `TerminalKeyModifiers`をこの層で複製したSwift型にラップし直さず直接受け渡すのは、
    /// 修飾キーの意味づけロジックをRust側だけに置く(rust-ssot)ため。省略時は修飾なし
    /// (既存呼び出し元との後方互換)。
    ///
    /// `kittyFlags`(Kitty keyboard protocol、タスク#54で交渉・`ScreenUpdate.kittyKeyboardFlags`
    /// として公開されるnegotiated flags)もRust側の`terminal_special_key_bytes`へそのまま
    /// 委譲する(タスク#72: 以前はこの引数配線が抜けており、交渉されたflagsが実際の送信
    /// バイト列に一切反映されない既存バグだった)。呼び出し側は取得できる最新値を渡すこと、
    /// 省略時は0(legacy mode、既存呼び出し元との後方互換)。
    public static func bytes(
        for key: SpecialKey,
        applicationCursorMode: Bool,
        modifiers: TerminalKeyModifiers = TerminalKeyModifiers(shift: false, alt: false, ctrl: false, meta: false),
        kittyFlags: UInt16 = 0
    ) -> [UInt8] {
        Array(terminalSpecialKeyBytes(key: key.rustKey, applicationCursorMode: applicationCursorMode, modifiers: modifiers, kittyFlags: kittyFlags))
    }

    /// Kitty keyboard protocolのbit0(disambiguate escape codes)有効時、Ctrl/Alt(併用・
    /// Shift+Alt含む)付きの印字可能文字キーをCSI u形式でエンコードする(タスク#91、
    /// Rust側`terminal_kitty_disambiguated_key_bytes`へそのまま委譲、rust-ssot)。
    /// `codePoint`はキーの無修飾時の基本コードポイントを渡すこと(大文字/小文字の
    /// 正規化はRust側が行う)。bit0が立っていない・Ctrl/Altどちらも押されていない・
    /// 印字可能文字でない場合は`nil`を返し、呼び出し側は既存の`controlByte`(legacy Ctrl)
    /// やESCプレフィックス(legacy Alt)へフォールバックすること。
    public static func kittyDisambiguatedKeyBytes(
        codePoint: UInt32,
        modifiers: TerminalKeyModifiers,
        kittyFlags: UInt16
    ) -> [UInt8]? {
        terminalKittyDisambiguatedKeyBytes(codePoint: codePoint, modifiers: modifiers, kittyFlags: kittyFlags).map(Array.init)
    }
}

private extension TerminalKeyMapper.SpecialKey {
    /// このSwift APIには`applicationCursorMode`の概念が無く常にCSI形式を返すため、
    /// `.backspace`はRust版の`Delete`(0x7F)に、`.delete`(前方削除)はAndroidに
    /// 存在しないRust版の`ForwardDelete`(`ESC[3~`)に対応する。
    var rustKey: TerminalSpecialKey {
        switch self {
        case .escape: return .escape
        case .tab: return .tab
        case .backspace: return .delete
        case .delete: return .forwardDelete
        case .arrowUp: return .arrowUp
        case .arrowDown: return .arrowDown
        case .arrowLeft: return .arrowLeft
        case .arrowRight: return .arrowRight
        case .home: return .home
        case .end: return .end
        case .pageUp: return .pageUp
        case .pageDown: return .pageDown
        case .functionKey(let n): return .functionKey(number: UInt8(clamping: n))
        }
    }
}
