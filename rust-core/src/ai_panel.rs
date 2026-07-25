//! AI統合機能の構造化パネル(`AI_INTEGRATION_DESIGN.md` §6.2)デコーダ。
//! `kitty_graphics.rs`の兄弟モジュール——同じ`ApcInterceptor`(`ESC _ … ST`)が
//! 切り出したペイロードを受け取るが、名前空間はペイロード先頭バイトで分岐する:
//! `G`始まりはKitty graphics(`kitty_graphics.rs`)、`{`始まり(生JSON)はこちら。
//! 新規のAPCチャンネルを増設するのではなく、既存`ApcInterceptor`の1つの出口を
//! 内容ベースで振り分けるだけ(Opusレビュー2026-07-24指摘、詳細は設計書参照)。
//!
//! ## 信頼境界
//! ペイロードはPTY上のin-bandであり、リモートの任意プロセス・`cat`した悪意ある
//! ファイル・curlの出力が偽造できる。したがってこのモジュールが公開する
//! [PanelField]/パネル内容には**実行権限を一切与えない**——値は常にただの表示用
//! テキストとして扱われ、シェルコマンド化・自動実行・クリップボード書き込み等の
//! 副作用を一切引き起こさない。フィードバック(フォーム送信結果)はPTYへの通常の
//! stdin文字列書き込みとして返す(`session.rs`側、新規exec RPCは追加しない)。
//!
//! ## スコープ(MVP)
//! - `presentDocument`: タイトル+Markdown本文の表示のみ。
//! - `presentForm`: テキスト入力/選択肢の2種類のフィールドのみ。
//! - 任意HTML/JS系プラグイン相当(MulmoTerminalのhtml-plugin等)はComposeに安全な
//!   相当物が無いため対象外(設計書§6.2)。
//! - 不正なJSON・未知の`type`・未知の`fields[].kind`は黙って無視する
//!   (`ApcInterceptor`と同じopportunisticな方針、画面を壊さない)。

use serde::Deserialize;

use crate::{PanelField, PanelFieldKind, PanelKind};

#[derive(Deserialize)]
#[serde(tag = "type")]
enum RawPanelEnvelope {
    #[serde(rename = "presentDocument")]
    Document { title: String, markdown: String },
    #[serde(rename = "presentForm")]
    Form { title: String, fields: Vec<RawPanelField> },
}

#[derive(Deserialize)]
struct RawPanelField {
    id: String,
    label: String,
    kind: String,
    #[serde(default)]
    options: Vec<String>,
}

/// [parse_ai_panel_apc]の成功時の戻り値。`Terminal::set_panel_from_ai_apc`が
/// そのままフィールドへ書き込む(`kind`によって`markdown`/`fields`のどちらが
/// 意味を持つかが決まる、`ScreenUpdate`側のdocコメント参照)。
pub(crate) struct ParsedPanel {
    pub(crate) kind: PanelKind,
    pub(crate) title: String,
    pub(crate) markdown: String,
    pub(crate) fields: Vec<PanelField>,
}

/// APCペイロードをAIパネルエンベロープとしてパースする。`payload`の先頭バイトが
/// `{`でなければ(Kitty graphicsの`G`始まり等、このモジュールの名前空間外)は
/// 即座に`None`を返す——`kitty_graphics.rs::KittyGraphics::dispatch`が`G`始まりを
/// 前提にしているのと対称の判定。
pub(crate) fn parse_ai_panel_apc(payload: &[u8]) -> Option<ParsedPanel> {
    if payload.first() != Some(&b'{') {
        return None;
    }
    let raw: RawPanelEnvelope = serde_json::from_slice(payload).ok()?;
    match raw {
        RawPanelEnvelope::Document { title, markdown } => {
            Some(ParsedPanel { kind: PanelKind::Document, title, markdown, fields: Vec::new() })
        }
        RawPanelEnvelope::Form { title, fields } => {
            let mut parsed_fields = Vec::with_capacity(fields.len());
            for f in fields {
                let kind = match f.kind.as_str() {
                    "text" => PanelFieldKind::Text,
                    "choice" => PanelFieldKind::Choice,
                    // 未知のfield kindはパネル全体を不正とみなし黙って無視する
                    // (中途半端な部分描画で誤操作を誘発するより安全側)。
                    _ => return None,
                };
                parsed_fields.push(PanelField { id: f.id, label: f.label, kind, options: f.options });
            }
            Some(ParsedPanel { kind: PanelKind::Form, title, markdown: String::new(), fields: parsed_fields })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_kitty_graphics_payload() {
        assert!(parse_ai_panel_apc(b"Ga=T,f=100;aGVsbG8=").is_none());
    }

    #[test]
    fn ignores_non_json_payload() {
        assert!(parse_ai_panel_apc(b"not json at all").is_none());
    }

    #[test]
    fn parses_present_document() {
        let payload = br#"{"type":"presentDocument","title":"Deploy Summary","markdown":"**ok**"}"#;
        let parsed = parse_ai_panel_apc(payload).unwrap();
        assert_eq!(parsed.kind, PanelKind::Document);
        assert_eq!(parsed.title, "Deploy Summary");
        assert_eq!(parsed.markdown, "**ok**");
        assert!(parsed.fields.is_empty());
    }

    #[test]
    fn parses_present_form_with_text_and_choice_fields() {
        let payload = br#"{
            "type": "presentForm",
            "title": "Confirm rename",
            "fields": [
                {"id": "name", "label": "New name", "kind": "text"},
                {"id": "env", "label": "Environment", "kind": "choice", "options": ["staging", "prod"]}
            ]
        }"#;
        let parsed = parse_ai_panel_apc(payload).unwrap();
        assert_eq!(parsed.kind, PanelKind::Form);
        assert_eq!(parsed.title, "Confirm rename");
        assert_eq!(parsed.fields.len(), 2);
        assert_eq!(parsed.fields[0].id, "name");
        assert_eq!(parsed.fields[0].kind, PanelFieldKind::Text);
        assert!(parsed.fields[0].options.is_empty());
        assert_eq!(parsed.fields[1].id, "env");
        assert_eq!(parsed.fields[1].kind, PanelFieldKind::Choice);
        assert_eq!(parsed.fields[1].options, vec!["staging".to_string(), "prod".to_string()]);
    }

    #[test]
    fn rejects_form_with_unknown_field_kind() {
        let payload = br#"{"type":"presentForm","title":"t","fields":[{"id":"x","label":"X","kind":"slider"}]}"#;
        assert!(parse_ai_panel_apc(payload).is_none());
    }

    #[test]
    fn rejects_unknown_type() {
        let payload = br#"{"type":"presentChart","title":"t"}"#;
        assert!(parse_ai_panel_apc(payload).is_none());
    }

    #[test]
    fn rejects_malformed_json() {
        let payload = br#"{"type":"presentDocument","title":"#;
        assert!(parse_ai_panel_apc(payload).is_none());
    }
}
