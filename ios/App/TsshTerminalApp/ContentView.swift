import SwiftUI
import TsshCore
import TsshCoreLogic

/// Phase 1A-1: rust-coreをSwiftUIアプリからリンクできること、Rustの同期/非同期関数呼び出し・
/// callback受信・Rustオブジェクトの明示的な破棄が一通り動くことを確認するための最小画面。
/// SSH接続そのものはここに含めない(実SSH縦切りは#20aで別途検証する)。
struct ContentView: View {
    @State private var versionText: String = "..."
    @State private var pingText: String = "..."
    @State private var callbackText: String = "..."
    @State private var diagnosticHandle: DiagnosticHandle?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("tssh-core version: \(versionText)")
            Text("core_ping(): \(pingText)")
            Text("callback: \(callbackText)")
        }
        .padding()
        .task {
            await runDiagnostics()
        }
    }

    private func runDiagnostics() async {
        versionText = coreVersion()
        pingText = await corePing()

        let handle = DiagnosticHandle()
        diagnosticHandle = handle
        let bridge = DiagnosticCallbackBridge { message in
            Task { @MainActor in
                callbackText = message
            }
        }
        handle.fireCallback(callback: bridge)

        // Rustオブジェクトを明示的に破棄する(Phase 1A-1の受け入れ条件)。
        diagnosticHandle = nil
    }
}

#Preview {
    ContentView()
}
