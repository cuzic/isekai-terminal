// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "TsshCore",
    platforms: [.iOS(.v15)],
    products: [
        .library(name: "TsshCore", targets: ["TsshCore"])
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
            dependencies: ["TsshCoreFFIBinary"],
            path: "Sources/TsshCore"
        ),
        .testTarget(
            name: "TsshCoreTests",
            dependencies: ["TsshCore"],
            path: "Tests/TsshCoreTests"
        ),
    ]
)
