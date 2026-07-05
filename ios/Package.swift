// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "IsekaiTerminalCore",
    platforms: [.iOS(.v15)],
    products: [
        .library(name: "IsekaiTerminalCore", targets: ["IsekaiTerminalCore"])
    ],
    targets: [
        // rust-core/scripts/build-ios-xcframework.sh が生成する。
        // Rust静的ライブラリ + Cヘッダー/modulemapのみを格納し、UniFFI生成の
        // Swiftソースはここに焼き込まず下の IsekaiTerminalCore ターゲット(source target)側に置く
        // （Swiftコンパイラのバージョン差分の影響を減らすため）。
        .binaryTarget(
            name: "IsekaiTerminalCoreFFIBinary",
            path: "Frameworks/IsekaiTerminalCoreFFI.xcframework"
        ),
        .target(
            name: "IsekaiTerminalCore",
            dependencies: ["IsekaiTerminalCoreFFIBinary"],
            path: "Sources/IsekaiTerminalCore"
        ),
        .testTarget(
            name: "IsekaiTerminalCoreTests",
            dependencies: ["IsekaiTerminalCore"],
            path: "Tests/IsekaiTerminalCoreTests"
        ),
    ]
)
