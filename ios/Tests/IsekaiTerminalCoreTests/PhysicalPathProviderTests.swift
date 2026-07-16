import Darwin
import XCTest
@testable import IsekaiTerminalCore

/// Phase 9-6(#15): CI(macOS Simulator)にはWiFi/セルラーどちらも実機と同じ形の物理
/// インターフェースがあるとは限らない(Simulatorはホストのネットワークを仮想化しており、
/// 特にセルラーは物理ハードウェアが無ければ確実に取得できない)。そのため各テストは
/// 「取得できた場合はfdが有効であること」「取得できなかった場合はnilを返し、クラッシュ
/// しないこと」の両方を許容し、実機での実際の復帰動作の検証は#17に委ねる。
final class PhysicalPathProviderTests: XCTestCase {
    func testAcquireWifiFdReturnsUsableFdOrNilGracefully() {
        let provider = PhysicalPathProvider()
        guard let (fd, localIp) = provider.acquireWifiFd(timeout: 3) else {
            return
        }
        defer { close(fd) }
        XCTAssertGreaterThanOrEqual(fd, 0)
        XCTAssertFalse(localIp.isEmpty)
        // 取得したfdが実際に有効(まだcloseされていない)ことを`fcntl`で確認する。
        XCTAssertNotEqual(fcntl(fd, F_GETFD), -1)
    }

    func testAcquireCellularFdReturnsUsableFdOrNilGracefully() {
        let provider = PhysicalPathProvider()
        guard let (fd, localIp) = provider.acquireCellularFd(timeout: 3) else {
            return
        }
        defer { close(fd) }
        XCTAssertGreaterThanOrEqual(fd, 0)
        XCTAssertFalse(localIp.isEmpty)
        XCTAssertNotEqual(fcntl(fd, F_GETFD), -1)
    }

    /// 複数回連続で呼んでも(fd所有権ポリシー: 毎回新規取得)クラッシュしたり
    /// 以前取得したfdを壊したりしないことを確認する。
    func testRepeatedAcquisitionsAreIndependent() {
        let provider = PhysicalPathProvider()
        var acquiredFds: [Int32] = []
        defer { acquiredFds.forEach { close($0) } }

        for _ in 0..<3 {
            if let (fd, _) = provider.acquireWifiFd(timeout: 3) {
                XCTAssertFalse(acquiredFds.contains(fd), "同じfdが再利用されるべきではない")
                acquiredFds.append(fd)
            }
        }
    }
}
