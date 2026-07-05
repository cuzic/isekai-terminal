import SwiftUI
import UIKit

/// Phase 1D(#18b): ターミナル本画面。SSH接続・VTE画面(`ScreenUpdate`)描画・
/// 日本語IME統合・特殊キーのアクセサリバーを1画面にまとめる。
///
/// cols/rowsは現時点では固定(80x24)。実際のview sizeやDynamic Type設定に応じた
/// 動的リサイズ(`SshSession.resize(cols:rows:)`は既に存在する)は後続の改善候補。
public struct TerminalView: View {
    @State private var controller: TerminalSessionController
    @ObservedObject private var uiState: TerminalUIState

    public init(
        profile: ConnectionProfile,
        password: String?,
        trustStore: SshHostTrustStore = AppServices.shared.trustStore
    ) {
        let c = TerminalSessionController(profile: profile, password: password, trustStore: trustStore)
        _controller = State(initialValue: c)
        _uiState = ObservedObject(wrappedValue: c.uiState)
    }

    public var body: some View {
        ZStack(alignment: .topLeading) {
            TerminalScreenRepresentable(uiState: uiState)
                .accessibilityIdentifier("terminalScreen")

            TerminalInputRepresentable(controller: controller, uiState: uiState)
                .frame(width: 1, height: 1)
                .opacity(0.01) // 非表示にしつつfirstResponderにはなれる状態を保つ

            statusOverlay
        }
        .background(Color.black)
        .navigationBarTitleDisplayMode(.inline)
        .onAppear { controller.connect() }
        .onDisappear { controller.disconnect() }
    }

    @ViewBuilder
    private var statusOverlay: some View {
        switch uiState.state {
        case .connecting:
            VStack {
                ProgressView()
                Text("接続中…").foregroundStyle(.white)
            }
            .accessibilityIdentifier("terminalConnectingOverlay")
        case .connected:
            EmptyView()
        case .disconnected(let reason):
            Text(reason.map { "切断されました: \($0)" } ?? "切断されました")
                .foregroundStyle(.white)
                .padding()
                .background(.black.opacity(0.7))
                .accessibilityIdentifier("terminalDisconnectedOverlay")
        case .failed(let message):
            Text("エラー: \(message)")
                .foregroundStyle(.red)
                .padding()
                .background(.black.opacity(0.7))
                .accessibilityIdentifier("terminalErrorOverlay")
        }
    }
}

private struct TerminalScreenRepresentable: UIViewRepresentable {
    @ObservedObject var uiState: TerminalUIState

    func makeUIView(context: Context) -> TerminalScreenView {
        TerminalScreenView()
    }

    func updateUIView(_ uiView: TerminalScreenView, context: Context) {
        if let update = uiState.latestScreenUpdate {
            uiView.apply(update)
        }
    }
}

private struct TerminalInputRepresentable: UIViewRepresentable {
    let controller: TerminalSessionController
    @ObservedObject var uiState: TerminalUIState

    func makeUIView(context: Context) -> TerminalIMEInputView {
        let view = TerminalIMEInputView()
        view.onSendBytes = { [weak controller] data in controller?.send(data) }
        view.inputAccessoryView = TerminalAccessoryBar(controller: controller, inputView: view)
        DispatchQueue.main.async {
            view.becomeFirstResponder()
        }
        return view
    }

    func updateUIView(_ uiView: TerminalIMEInputView, context: Context) {
        uiView.bracketedPasteMode = uiState.latestScreenUpdate?.bracketedPasteMode ?? false
    }
}

/// 特殊キー(Ctrl/Esc/Tab/矢印/Home/End/PageUp/PageDown)用のキーボードアクセサリバー。
/// 矢印以外は`TerminalKeyMapper`(rust-core委譲、Android版と共通のバイト列)を使う。
/// `applicationCursorMode`切り替えはSwift版`TerminalKeyMapper`のAPIには無いため
/// (常にCSI形式)、矢印キーはこのアクセサリバーではapplication cursor modeを
/// 考慮しない(既知の制約、PLAN.md参照)。
///
/// 「Ctrl」ボタンはトグル式: ONにした状態で次にソフトウェアキーボードで入力された
/// 1文字を、`TerminalIMEInputView.ctrlArmed`経由でCtrl制御バイトに変換して送信する。
private final class TerminalAccessoryBar: UIView {
    private weak var controller: TerminalSessionController?
    // `UIResponder.inputView`という既存プロパティと名前が衝突し
    // 「'strong'プロパティを'weak'でオーバーライドできない」エラーになるため、
    // `imeInputView`という別名にする。
    private weak var imeInputView: TerminalIMEInputView?
    private var ctrlButton: UIButton?

    init(controller: TerminalSessionController, inputView: TerminalIMEInputView) {
        self.controller = controller
        self.imeInputView = inputView
        super.init(frame: CGRect(x: 0, y: 0, width: 0, height: 44))
        backgroundColor = .secondarySystemBackground
        autoresizingMask = [.flexibleWidth]

        let ctrl = makeButton(title: "Ctrl", tag: -1)
        ctrl.addTarget(self, action: #selector(handleCtrlTap), for: .touchUpInside)
        ctrlButton = ctrl

        let labels: [(String, TerminalKeyMapper.SpecialKey)] = [
            ("Esc", .escape),
            ("Tab", .tab),
            ("↑", .arrowUp),
            ("↓", .arrowDown),
            ("←", .arrowLeft),
            ("→", .arrowRight),
            ("Home", .home),
            ("End", .end),
            ("PgUp", .pageUp),
            ("PgDn", .pageDown),
        ]
        self.keys = labels.map { $0.1 }

        let keyButtons = labels.enumerated().map { index, item in makeButton(title: item.0, tag: index) }
        let stack = UIStackView(arrangedSubviews: [ctrl] + keyButtons)
        stack.axis = .horizontal
        stack.distribution = .fillEqually
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)

        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor),
            stack.topAnchor.constraint(equalTo: topAnchor),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    private var keys: [TerminalKeyMapper.SpecialKey] = []

    private func makeButton(title: String, tag: Int) -> UIButton {
        var config = UIButton.Configuration.plain()
        config.title = title
        config.contentInsets = NSDirectionalEdgeInsets(top: 8, leading: 4, bottom: 8, trailing: 4)
        let button = UIButton(configuration: config)
        button.tag = tag
        button.accessibilityIdentifier = "terminalAccessory_\(title)"
        button.addTarget(self, action: #selector(handleTap(_:)), for: .touchUpInside)
        return button
    }

    @objc private func handleCtrlTap() {
        guard let imeInputView else { return }
        imeInputView.ctrlArmed.toggle()
        ctrlButton?.configuration?.baseBackgroundColor = imeInputView.ctrlArmed ? .systemBlue : nil
    }

    @objc private func handleTap(_ sender: UIButton) {
        guard keys.indices.contains(sender.tag) else { return }
        let bytes = TerminalKeyMapper.bytes(for: keys[sender.tag])
        controller?.send(Data(bytes))
    }
}
