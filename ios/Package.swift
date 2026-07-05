// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "TsshCore",
    // Phase 1D: NavigationStack/.navigationDestination(for:)がiOS 16+必須のため15→16へ引き上げ。
    platforms: [.iOS(.v16)],
    products: [
        .library(name: "TsshCore", targets: ["TsshCore"])
    ],
    dependencies: [
        // 接続プロファイル管理(#10)のローカル永続化に使う。Android版Roomと
        // 概念的に対称なDatabaseMigrator(明示的マイグレーション管理)を持つため
        // 第一候補として採用(ChatGPT外部レビュー2026-07-04、PLAN.md「Phase Y」節)。
        .package(url: "https://github.com/groue/GRDB.swift.git", from: "6.0.0"),
    ],
    targets: [
        // rust-core/scripts/build-ios-xcframework.sh が生成する。
        // Rust静的ライブラリ + Cヘッダー/modulemapのみを格納し、UniFFI生成の
        // Swiftソースはここに焼き込まず下の TsshCore ターゲット(source target)側に置く
        // （Swiftコンパイラのバージョン差分の影響を減らすため）。
        .binaryTarget(
            name: "TsshCoreFFIBinary",
            path: "Frameworks/TsshCoreFFI.xcframework"
        ),
        .target(
            name: "TsshCore",
            dependencies: [
                "TsshCoreFFIBinary",
                .product(name: "GRDB", package: "GRDB.swift"),
            ],
            path: "Sources/TsshCore"
        ),
        .testTarget(
            name: "TsshCoreTests",
            dependencies: ["TsshCore"],
            path: "Tests/TsshCoreTests"
        ),
    ]
)
