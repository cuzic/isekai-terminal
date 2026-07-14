import Darwin
import Foundation
import Network

/// Phase 9-6(#15): WiFi/セルラー物理インターフェースにそれぞれ`IP_BOUND_IF`で明示的に
/// バインドしたUDPソケットの生fdを取得する。Android版
/// `PhysicalPathProvider.acquireWifiOnly()`/`acquireCellularOnly()`(#10/#20)のiOS版で、
/// `Network.bindSocket()`に相当する処理をBSDソケットAPIで行う。
///
/// 判断ロジックは一切持たない(`.claude/rules/rust-ssot.md`準拠) — 呼ばれたら該当種別の
/// インターフェースへバインドしたfdを取得して返すだけで、いつ呼ぶか・取得できなかった
/// 場合にどうするか(セルラーへのフェイルオーバー可否等)は`RebindManager`(Rust側の純粋
/// 状態機械、rebind_manager.rs)が決める。
///
/// Android版が`ConnectivityManager.requestNetwork`の`onAvailable`をタイムアウト付きで
/// 待つのと対称に、ここでは`NWPathMonitor`が対象種別のインターフェースを含む`NWPath`を
/// 報告するのをタイムアウト付きで待つ。fd取得自体は低頻度(WiFi復帰の疎通確認・実際の
/// rebind時のみ)なので、呼び出しごとに使い捨ての`NWPathMonitor`を起動・破棄する
/// (長命の監視状態を持ち回さない)。
///
/// IPv4のみ対応(IPv6-onlyネットワークは未対応、Android版`PhysicalPathProvider`の
/// `bindAndDetach`と同じ制約)。Simulator上ではネットワークがホストを介して仮想化
/// されているため、`IP_BOUND_IF`によるインターフェース分離が実機と同じように機能
/// するとは限らない(Task #15のサブタスク、実機での検証は#17が担う)。
final class PhysicalPathProvider {
    /// WiFiだけをbindしたfd+ローカルIPを取得する。取得できなければ`nil`
    /// (WiFi自体が使えない・タイムアウト等、正常系として扱う)。
    func acquireWifiFd(timeout: TimeInterval = 5) -> (fd: Int32, localIp: String)? {
        acquireOne(interfaceType: .wifi, timeout: timeout)
    }

    /// セルラーだけをbindしたfd+ローカルIPを取得する([acquireWifiFd]のセルラー版)。
    func acquireCellularFd(timeout: TimeInterval = 5) -> (fd: Int32, localIp: String)? {
        acquireOne(interfaceType: .cellular, timeout: timeout)
    }

    private func acquireOne(interfaceType: NWInterface.InterfaceType, timeout: TimeInterval) -> (Int32, String)? {
        guard let interface = Self.awaitInterface(ofType: interfaceType, timeout: timeout) else {
            return nil
        }
        return Self.bind(toInterface: interface)
    }

    /// `NWPathMonitor`が`interfaceType`を含む`NWPath`を報告するまで(または`timeout`まで)待つ。
    private static func awaitInterface(ofType interfaceType: NWInterface.InterfaceType, timeout: TimeInterval) -> NWInterface? {
        let monitor = NWPathMonitor()
        let semaphore = DispatchSemaphore(value: 0)
        let box = InterfaceBox()
        monitor.pathUpdateHandler = { path in
            if let match = path.availableInterfaces.first(where: { $0.type == interfaceType }) {
                box.interface = match
                semaphore.signal()
            }
        }
        monitor.start(queue: DispatchQueue(label: "tools.isekai.terminal.physical-path-provider"))
        _ = semaphore.wait(timeout: .now() + timeout)
        monitor.cancel()
        return box.interface
    }

    /// `setsockopt(IPPROTO_IP, IP_BOUND_IF)`で明示的にインターフェースへバインドし、
    /// そのインターフェースの実際のIPv4アドレスへ`bind(2)`する。ワイルドカードbind
    /// (`INADDR_ANY`)のままだと、Android版が実機検証で踏んだのと同じ理由
    /// (デュアルスタック環境で意図しないアドレスが選ばれ得る)で、呼び出し元
    /// (Rust側`RebindExecutor`)が期待する「このインターフェースの」ローカルIPと
    /// ずれる可能性があるため避ける。
    private static func bind(toInterface interface: NWInterface) -> (Int32, String)? {
        guard let ipv4 = ipv4Address(forInterfaceNamed: interface.name) else {
            return nil
        }

        let fd = socket(AF_INET, SOCK_DGRAM, 0)
        guard fd >= 0 else { return nil }

        var index = UInt32(interface.index)
        guard setsockopt(fd, IPPROTO_IP, IP_BOUND_IF, &index, socklen_t(MemoryLayout<UInt32>.size)) == 0 else {
            close(fd)
            return nil
        }

        var addr = sockaddr_in()
        addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = 0
        addr.sin_addr = ipv4
        let bindResult = withUnsafePointer(to: &addr) { addrPtr -> Int32 in
            addrPtr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockaddrPtr in
                Darwin.bind(fd, sockaddrPtr, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard bindResult == 0 else {
            close(fd)
            return nil
        }

        var addrForPrint = ipv4
        var buffer = [Int8](repeating: 0, count: Int(INET_ADDRSTRLEN))
        guard inet_ntop(AF_INET, &addrForPrint, &buffer, socklen_t(INET_ADDRSTRLEN)) != nil else {
            close(fd)
            return nil
        }
        return (fd, String(cString: buffer))
    }

    /// `getifaddrs(3)`でインターフェース名からIPv4アドレスを取得する。Android版が
    /// `LinkProperties.linkAddresses`から取得するのと同じ役割。
    private static func ipv4Address(forInterfaceNamed name: String) -> in_addr? {
        var ifaddrPtr: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&ifaddrPtr) == 0, let firstAddr = ifaddrPtr else { return nil }
        defer { freeifaddrs(ifaddrPtr) }

        var cursor: UnsafeMutablePointer<ifaddrs>? = firstAddr
        while let ifa = cursor {
            defer { cursor = ifa.pointee.ifa_next }
            guard let sa = ifa.pointee.ifa_addr, sa.pointee.sa_family == UInt8(AF_INET) else { continue }
            guard String(cString: ifa.pointee.ifa_name) == name else { continue }
            return sa.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { $0.pointee.sin_addr }
        }
        return nil
    }
}

/// `NWPathMonitor.pathUpdateHandler`(非isolatedクロージャ)から`DispatchSemaphore`越しに
/// 結果を受け渡すための小さな箱。`TerminalSessionController.swift`の
/// `AgentSignResultBox`と同じ設計(semaphoreのwait/signalが確立するhappens-before関係
/// により、実質的にアクセスが直列化されることを前提にしている)。
private final class InterfaceBox: @unchecked Sendable {
    var interface: NWInterface?
}
