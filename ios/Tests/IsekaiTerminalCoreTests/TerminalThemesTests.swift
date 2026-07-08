import XCTest
@testable import IsekaiTerminalCore

/// Phase 1F-3(#50): 配色テーマプリセットの検証。Android版`TerminalThemeTest.kt`相当。
final class TerminalThemesTests: XCTestCase {
    func testAllThemesHaveExactly16Ansi16Entries() {
        for theme in TerminalThemes.all {
            XCTAssertEqual(theme.ansi16.count, 16, "\(theme.name) has \(theme.ansi16.count) ansi16 entries")
        }
    }

    func testAllThemeNamesAreUnique() {
        let names = TerminalThemes.all.map(\.name)
        XCTAssertEqual(Set(names).count, names.count)
    }

    func testByNameResolvesKnownTheme() {
        XCTAssertEqual(TerminalThemes.byName("Dracula"), TerminalThemes.dracula)
        XCTAssertEqual(TerminalThemes.byName("Nord"), TerminalThemes.nord)
        XCTAssertEqual(TerminalThemes.byName("Solarized Dark"), TerminalThemes.solarizedDark)
    }

    func testByNameFallsBackToDefaultDarkForUnknownOrNil() {
        XCTAssertEqual(TerminalThemes.byName("does-not-exist"), TerminalThemes.defaultDark)
        XCTAssertEqual(TerminalThemes.byName(nil), TerminalThemes.defaultDark)
    }

    /// rust-core/src/theme.rsのTheme::default()と一致していることの後方互換確認
    /// (Android版`TerminalThemeTest.kt`の同種テストと同じ値をここでも固定する)。
    func testDefaultDarkMatchesRustCoreDefault() {
        XCTAssertEqual(TerminalThemes.defaultDark.foreground, 0xFFCCCCCC)
        XCTAssertEqual(TerminalThemes.defaultDark.background, 0xFF000000)
        XCTAssertEqual(TerminalThemes.defaultDark.ansi16.first, 0xFF000000)
        XCTAssertEqual(TerminalThemes.defaultDark.ansi16.last, 0xFFFFFFFF)
    }
}
