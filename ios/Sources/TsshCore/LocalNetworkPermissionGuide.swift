import Foundation
import UIKit
import TsshCoreLogic

/// Phase 1B: Local Network Privacy(LAN内`direct_address`への接続時に必要)の
/// 許可/拒否時のUI導線をサポートするヘルパー。
///
/// iOSにはLocal Network Privacyの許可状態を事前に問い合わせるAPIが無く、
/// 実際に接続を試みて失敗するまで拒否されているかどうかは分からない
/// (Photos/Cameraのような`authorizationStatus()`相当が存在しない)。そのため
/// ここでは「設定アプリへ誘導する」導線のみを提供し、拒否の検知自体は
/// 実際の接続エラー(Rust側のtransport error)をトリガーに呼び出し側が判断する。
///
/// Bonjour探索は使わずdirect IPへ接続するだけの設計のため、`NSBonjourServices`は
/// 追加しない(Bonjourを実際に導入する場合のみ追加する、ChatGPT外部レビュー
/// 2026-07-04参照)。
public enum LocalNetworkPermissionGuide {
    /// 設定アプリのこのアプリの設定画面を開くためのURL。値のハードコードは避け、
    /// 実際の`UIApplication.openSettingsURLString`をそのまま使う。
    public static var appSettingsURL: URL? {
        URL(string: UIApplication.openSettingsURLString)
    }

    /// 設定アプリのこのアプリの設定画面を開く。呼び出し元(SwiftUIのボタン等)から
    /// 「接続がLocal Network Privacyで拒否された可能性がある」場合に呼ぶ想定。
    @MainActor
    public static func openAppSettings() {
        guard let url = appSettingsURL else { return }
        UIApplication.shared.open(url)
    }
}
