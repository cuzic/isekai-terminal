// swift-tools-version:5.9
import PackageDescription

// SwiftPMはbinaryTarget(.xcframework)の実体存在チェックを、そのターゲットが
// 実際にビルドグラフに含まれるかどうかに関係なくマニフェスト読み込み時に行う。
// また`swift test`は宣言されている全testTargetを1つのテストプロダクトへ
// まとめてビルドしようとするため、`IsekaiTerminalCore`/`IsekaiTerminalCoreTests`(UIKit/SwiftUI/
// GRDB/CryptoKit依存)がマニフェストに存在するだけでLinux上の`swift test`が
// 巻き込まれて失敗する。そのため`#if os(Linux)`でマニフェスト自体から
// Apple専用ターゲットの宣言を丸ごと出し分ける(このディレクティブはクロス
// コンパイル先ではなく、`swift build`/`swift test`を実行しているホストOSで
// 評価される)。Linux上では`IsekaiTerminalCoreLogic`/`IsekaiTerminalCoreLogicTests`だけが存在する
// パッケージとして解決される。
#if os(Linux)
let ffiTargets: [Target] = [
    // rust-core/scripts/build-linux-swift-ffi.sh が生成する(Linux専用)。
    .systemLibrary(
        name: "IsekaiTerminalCoreFFILinux",
        path: "Sources/IsekaiTerminalCoreFFILinux"
    ),
]
let logicFFIDependencies: [Target.Dependency] = [
    .target(name: "IsekaiTerminalCoreFFILinux"),
    .product(name: "Crypto", package: "swift-crypto"),
]
let isekaiTerminalCoreLogicLinkerSettings: [LinkerSetting] = [
    // -L(検索パス)はunsafeFlagsになりXcodeでの非ルートパッケージ利用時に解決エラーに
    // なるため使わない。`LIBRARY_PATH`/`LD_LIBRARY_PATH`環境変数側で渡す運用にする
    // (rust-core/scripts/build-linux-swift-ffi.sh のコメント、
    // .github/workflows/ios-logic-linux-check.yml 参照)。
    .linkedLibrary("isekai_terminal_core"),
]
let products: [Product] = [
    .library(name: "IsekaiTerminalCoreLogic", targets: ["IsekaiTerminalCoreLogic"]),
]
let packageDependencies: [Package.Dependency] = [
    // KeyManager(ed25519生成)がLinuxで`CryptoKit`の代わりに使う。Apple platforms
    // では`#if canImport(CryptoKit)`分岐によりそもそも参照しないため、依存自体を
    // Linuxビルドにのみ追加している。
    .package(url: "https://github.com/apple/swift-crypto.git", from: "3.0.0"),
]
let appleOnlyTargets: [Target] = []
#else
let ffiTargets: [Target] = [
    // rust-core/scripts/build-ios-xcframework.sh が生成する(macOS専用、Apple platforms向け)。
    // Rust静的ライブラリ + Cヘッダー/modulemapのみを格納し、UniFFI生成の
    // Swiftソースはここに焼き込まず IsekaiTerminalCoreLogic ターゲット(source target)側に置く
    // （Swiftコンパイラのバージョン差分の影響を減らすため）。
    .binaryTarget(
        name: "IsekaiTerminalCoreFFIBinary",
        path: "Frameworks/IsekaiTerminalCoreFFI.xcframework"
    ),
]
let logicFFIDependencies: [Target.Dependency] = [
    .target(name: "IsekaiTerminalCoreFFIBinary"),
]
let isekaiTerminalCoreLogicLinkerSettings: [LinkerSetting] = []
let products: [Product] = [
    .library(name: "IsekaiTerminalCore", targets: ["IsekaiTerminalCore"]),
    // Linux(`swift test`)でも成立する、UIKit/SwiftUI/GRDB/Keychainに依存しない
    // 純ロジック層。Mozillaのrust-components-swiftと同じ考え方(Rust coreは
    // 分厚くテストし、Swift境界は薄い契約テストに絞る)で切り出した
    // (詳細はPLAN.md「Phase Y」節、iOS Linux CI導入の記録を参照)。
    .library(name: "IsekaiTerminalCoreLogic", targets: ["IsekaiTerminalCoreLogic"]),
]
let packageDependencies: [Package.Dependency] = [
    // 接続プロファイル管理(#10)のローカル永続化に使う。Android版Roomと
    // 概念的に対称なDatabaseMigrator(明示的マイグレーション管理)を持つため
    // 第一候補として採用(ChatGPT外部レビュー2026-07-04、PLAN.md「Phase Y」節)。
    .package(url: "https://github.com/groue/GRDB.swift.git", from: "6.0.0"),
]
let appleOnlyTargets: [Target] = [
    .target(
        name: "IsekaiTerminalCore",
        dependencies: [
            "IsekaiTerminalCoreLogic",
            .product(name: "GRDB", package: "GRDB.swift"),
        ],
        path: "Sources/IsekaiTerminalCore"
    ),
    .testTarget(
        name: "IsekaiTerminalCoreTests",
        dependencies: ["IsekaiTerminalCore"],
        path: "Tests/IsekaiTerminalCoreTests"
    ),
]
#endif

let package = Package(
    name: "IsekaiTerminalCore",
    // Phase 1D: NavigationStack/.navigationDestination(for:)がiOS 16+必須のため15→16へ引き上げ。
    // (Linuxビルド/テストはこの`platforms`指定の対象外であり、影響しない。)
    platforms: [.iOS(.v16)],
    products: products,
    dependencies: packageDependencies,
    targets: ffiTargets + appleOnlyTargets + [
        .target(
            name: "IsekaiTerminalCoreLogic",
            dependencies: logicFFIDependencies,
            path: "Sources/IsekaiTerminalCoreLogic",
            exclude: [
                "generated/isekai_terminal_core.swift.sha256",
                "generated/isekai_terminal_coreFFI.h.sha256",
                "generated/isekai_terminal_coreFFI.modulemap.sha256",
            ],
            linkerSettings: isekaiTerminalCoreLogicLinkerSettings
        ),
        .testTarget(
            name: "IsekaiTerminalCoreLogicTests",
            dependencies: ["IsekaiTerminalCoreLogic"],
            path: "Tests/IsekaiTerminalCoreLogicTests"
        ),
    ]
)
