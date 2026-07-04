import Foundation

/// Phase 1B: ターミナル特殊キー操作(#11c/d)のうち、実機を必要としない
/// 「キー→制御シーケンス変換」ロジックの部分。キーボードアクセサリバーの
/// 見た目・レイアウトや選択/コピー/ペーストのUI・Dynamic Typeとは独立した
/// フォントサイズ設定UIは実機/シミュレータでの目視確認が必要なため、
/// この実装のスコープには含めない(PLAN.md「Phase Y」節参照)。
public enum TerminalKeyMapper {
    /// Ctrl+<英字>を対応する制御バイト(0x01〜0x1A)に変換する。
    /// 大文字・小文字どちらの入力でも同じ結果になる(実際のCtrlキーの挙動に合わせる)。
    public static func controlByte(for letter: Character) -> UInt8? {
        guard let ascii = letter.asciiValue else { return nil }
        let upper: UInt8
        if ascii >= 97 && ascii <= 122 {
            upper = ascii - 32 // a-z -> A-Z
        } else {
            upper = ascii
        }
        guard upper >= 65 && upper <= 90 else { return nil } // A-Z
        return upper - 64 // A=0x01 ... Z=0x1A
    }

    public enum SpecialKey: Equatable {
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
    public static func bytes(for key: SpecialKey) -> [UInt8] {
        switch key {
        case .escape: return [0x1B]
        case .tab: return [0x09]
        case .backspace: return [0x7F]
        case .delete: return Array("\u{1B}[3~".utf8)
        case .arrowUp: return Array("\u{1B}[A".utf8)
        case .arrowDown: return Array("\u{1B}[B".utf8)
        case .arrowRight: return Array("\u{1B}[C".utf8)
        case .arrowLeft: return Array("\u{1B}[D".utf8)
        case .home: return Array("\u{1B}[H".utf8)
        case .end: return Array("\u{1B}[F".utf8)
        case .pageUp: return Array("\u{1B}[5~".utf8)
        case .pageDown: return Array("\u{1B}[6~".utf8)
        case .functionKey(let n): return functionKeySequence(n)
        }
    }

    /// xterm互換の代表的なF1〜F12マッピング。F1〜F4はSS3(`ESC O`)形式、
    /// F5以降はCSI `~`形式(F16相当のCSIコードとの衝突を避けるためF5=15番から開始)。
    private static func functionKeySequence(_ n: Int) -> [UInt8] {
        switch n {
        case 1: return Array("\u{1B}OP".utf8)
        case 2: return Array("\u{1B}OQ".utf8)
        case 3: return Array("\u{1B}OR".utf8)
        case 4: return Array("\u{1B}OS".utf8)
        case 5: return Array("\u{1B}[15~".utf8)
        case 6: return Array("\u{1B}[17~".utf8)
        case 7: return Array("\u{1B}[18~".utf8)
        case 8: return Array("\u{1B}[19~".utf8)
        case 9: return Array("\u{1B}[20~".utf8)
        case 10: return Array("\u{1B}[21~".utf8)
        case 11: return Array("\u{1B}[23~".utf8)
        case 12: return Array("\u{1B}[24~".utf8)
        default: return []
        }
    }
}
