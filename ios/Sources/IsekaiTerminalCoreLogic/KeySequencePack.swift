import Foundation

/// 打鍵列セット(パック)のプレースホルダーパラメータ定義。例: tmuxパックの"prefix"
/// (ラベル「tmuxプレフィックスキー」、既定値 Ctrl+B)。Android版`PackParam`と対称。
public struct KeySequencePackParam: Equatable {
    public let name: String
    public let label: String
    public let defaultStep: KeyStep

    public init(name: String, label: String, defaultStep: KeyStep) {
        self.name = name
        self.label = label
        self.defaultStep = defaultStep
    }
}

/// パック内の1打鍵列テンプレート。[steps]は[KeyStep.placeholderRef]を含みうる。
/// Android版`PackSequenceTemplate`と対称。
public struct KeySequencePackTemplate: Equatable {
    public let label: String
    public let steps: [KeyStep]

    public init(label: String, steps: [KeyStep]) {
        self.label = label
        self.steps = steps
    }
}

/// 打鍵列セット(パック)の静的定義。DB行ではなくアプリに同梱するコンテンツ。
/// このデータモデル自体はtmux等の特定ソフトウェアの知識を持たない汎用エンジン
/// (エンジンが知っているのは「プレースホルダーを持つ打鍵列テンプレートの集合」のみ)。
/// Android版`KeySequencePack`と対称。
///
/// [version]はライブバインディング解決ロジックの分岐には使わない(表示・デバッグ用の記録に
/// 留める)。破壊的変更が必要な場合は新しい[id]を発行する方針。
public struct KeySequencePack: Equatable {
    public let id: String
    public let version: Int
    public let name: String
    public let params: [KeySequencePackParam]
    public let sequences: [KeySequencePackTemplate]

    public init(id: String, version: Int, name: String, params: [KeySequencePackParam], sequences: [KeySequencePackTemplate]) {
        self.id = id
        self.version = version
        self.name = name
        self.params = params
        self.sequences = sequences
    }
}

/// アプリに同梱するパック一覧。MVPではtmuxパック1つのみ。Android版`KeySequencePacks`と対称、
/// パック内容(tmuxのsequence一覧・ラベル・prefixデフォルト)を一致させること。
public enum KeySequencePacks {
    public static let tmux = KeySequencePack(
        id: "tmux",
        version: 1,
        name: "tmux",
        params: [
            KeySequencePackParam(name: "prefix", label: "tmuxプレフィックスキー", defaultStep: .ctrlChar("b")),
        ],
        sequences: [
            // tmuxの`%`/`"`は「縦分割」「横分割」という呼び方だと分割線の向きか配置の向きかで
            // 解釈が割れるため、明確な「左右」「上下」表記にする(Android版と同じ判断)。
            // `%`(split-window -h)は左右に並べる、`"`(split-window -v)は上下に並べる。
            KeySequencePackTemplate(label: "新規ウィンドウ", steps: [.placeholderRef("prefix"), .text("c")]),
            KeySequencePackTemplate(label: "ペイン分割(左右)", steps: [.placeholderRef("prefix"), .text("%")]),
            KeySequencePackTemplate(label: "ペイン分割(上下)", steps: [.placeholderRef("prefix"), .text("\"")]),
            KeySequencePackTemplate(label: "次のペイン", steps: [.placeholderRef("prefix"), .text("o")]),
            KeySequencePackTemplate(label: "デタッチ", steps: [.placeholderRef("prefix"), .text("d")]),
        ]
    )

    public static let all: [KeySequencePack] = [tmux]

    public static func find(id: String) -> KeySequencePack? {
        all.first { $0.id == id }
    }
}
