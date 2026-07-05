import SwiftUI
import TsshCoreLogic

/// Phase 1F-3(#50): ターミナル配色テーマ(プリセット選択・永続化)。Android版
/// `ui/TerminalTheme.kt`のSwift移植。
///
/// SGR解釈テーブル自体はRust側(`rust-core/src/theme.rs`)がグローバル状態として保持し、
/// `setTerminalTheme`経由で差し替える。この型はSwift側でのプリセット定義・永続化キー・
/// Rustへ渡すARGBパック値の保持のみを担う。呼び出し以降にパースされるSGRにのみ反映され、
/// 既にscrollbackに積まれた行は遡って再着色されない(既知の制約、Android版と同じ)。
public struct TerminalTheme: Equatable {
    public let name: String
    public let foreground: UInt32
    public let background: UInt32
    public let cursor: UInt32
    public let ansi16: [UInt32]

    public init(name: String, foreground: UInt32, background: UInt32, cursor: UInt32, ansi16: [UInt32]) {
        precondition(ansi16.count == 16, "ansi16 must have exactly 16 entries, got \(ansi16.count)")
        self.name = name
        self.foreground = foreground
        self.background = background
        self.cursor = cursor
        self.ansi16 = ansi16
    }

    /// プロファイル一覧・編集画面でのプレビュー表示用。
    public var backgroundColor: Color { Color(argb: background) }
    public var foregroundColor: Color { Color(argb: foreground) }

    /// Rust側`setTerminalTheme`へこのテーマを適用する。
    public func apply() {
        setTerminalTheme(ansi16: ansi16, defaultFg: foreground, defaultBg: background)
    }
}

extension Color {
    init(argb: UInt32) {
        let a = Double((argb >> 24) & 0xFF) / 255.0
        let r = Double((argb >> 16) & 0xFF) / 255.0
        let g = Double((argb >> 8) & 0xFF) / 255.0
        let b = Double(argb & 0xFF) / 255.0
        self.init(.sRGB, red: r, green: g, blue: b, opacity: a == 0 ? 1.0 : a)
    }
}

public enum TerminalThemes {
    /// `UserDefaults`に保存するテーマ名のキー(Android版`SharedPreferences("tssh_ui")`の
    /// `PREF_KEY`と同じキー名)。
    public static let prefKey = "terminal_theme"

    // 既定ダーク: rust-core/src/theme.rsのTheme::default()と同じ値を維持する
    // (既存のVTEユニットテスト・見た目の後方互換のため必ず一致させること、Android版と同じ)。
    public static let defaultDark = TerminalTheme(
        name: "Default Dark",
        foreground: 0xFFCCCCCC, background: 0xFF000000, cursor: 0xFFFFFFFF,
        ansi16: [
            0xFF000000, 0xFFAA0000, 0xFF00AA00, 0xFFAAAA00,
            0xFF0000AA, 0xFFAA00AA, 0xFF00AAAA, 0xFFAAAAAA,
            0xFF555555, 0xFFFF5555, 0xFF55FF55, 0xFFFFFF55,
            0xFF5555FF, 0xFFFF55FF, 0xFF55FFFF, 0xFFFFFFFF,
        ]
    )

    // Solarized Dark 公式パレット出典: https://ethanschoonover.com/solarized/
    public static let solarizedDark = TerminalTheme(
        name: "Solarized Dark",
        foreground: 0xFF839496, background: 0xFF002B36, cursor: 0xFF93A1A1,
        ansi16: [
            0xFF073642, 0xFFDC322F, 0xFF859900, 0xFFB58900,
            0xFF268BD2, 0xFFD33682, 0xFF2AA198, 0xFFEEE8D5,
            0xFF002B36, 0xFFCB4B16, 0xFF586E75, 0xFF657B83,
            0xFF839496, 0xFF6C71C4, 0xFF93A1A1, 0xFFFDF6E3,
        ]
    )

    // Dracula 公式ターミナル配色 出典: https://draculatheme.com/contribute (Terminal palette)
    public static let dracula = TerminalTheme(
        name: "Dracula",
        foreground: 0xFFF8F8F2, background: 0xFF282A36, cursor: 0xFFF8F8F2,
        ansi16: [
            0xFF21222C, 0xFFFF5555, 0xFF50FA7B, 0xFFF1FA8C,
            0xFFBD93F9, 0xFFFF79C6, 0xFF8BE9FD, 0xFFF8F8F2,
            0xFF6272A4, 0xFFFF6E6E, 0xFF69FF94, 0xFFFFFFA5,
            0xFFD6ACFF, 0xFFFF92DF, 0xFFA4FFFF, 0xFFFFFFFF,
        ]
    )

    // Nord 公式パレット 出典: https://www.nordtheme.com/docs/colors-and-palettes
    public static let nord = TerminalTheme(
        name: "Nord",
        foreground: 0xFFD8DEE9, background: 0xFF2E3440, cursor: 0xFFD8DEE9,
        ansi16: [
            0xFF3B4252, 0xFFBF616A, 0xFFA3BE8C, 0xFFEBCB8B,
            0xFF81A1C1, 0xFFB48EAD, 0xFF88C0D0, 0xFFE5E9F0,
            0xFF4C566A, 0xFFBF616A, 0xFFA3BE8C, 0xFFEBCB8B,
            0xFF81A1C1, 0xFFB48EAD, 0xFF8FBCBB, 0xFFECEFF4,
        ]
    )

    public static let all: [TerminalTheme] = [defaultDark, solarizedDark, dracula, nord]

    /// プリセット名からテーマを解決する。未知の名前・nilの場合は既定ダークにフォールバックする。
    public static func byName(_ name: String?) -> TerminalTheme {
        all.first { $0.name == name } ?? defaultDark
    }
}
