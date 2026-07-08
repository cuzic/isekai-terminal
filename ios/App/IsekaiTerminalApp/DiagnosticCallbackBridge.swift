import IsekaiTerminalCore
import IsekaiTerminalCoreLogic

/// `DiagnosticCallback`(Rust `callback_interface`)をSwiftのクロージャで
/// 実装するための薄いブリッジ。Phase 1A-1のsmoke testでのみ使う診断用コードで、
/// セッション/接続の状態は一切持たない。
final class DiagnosticCallbackBridge: DiagnosticCallback {
    private let onEvent: (String) -> Void

    init(onEvent: @escaping (String) -> Void) {
        self.onEvent = onEvent
    }

    func onDiagnosticEvent(message: String) {
        onEvent(message)
    }
}
