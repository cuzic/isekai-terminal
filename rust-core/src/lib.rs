uniffi::setup_scaffolding!("isekai_terminal_core");

pub mod trzsz;
pub mod file_preview;
pub mod quic_transport;
pub(crate) mod agent_forward;
pub(crate) mod terminal;
pub(crate) mod sixel;
pub(crate) mod kitty_graphics;
pub(crate) mod theme;
pub(crate) mod transport;
pub(crate) mod pool;
pub(crate) mod socks;
pub(crate) mod session_state;
pub(crate) mod session;
pub(crate) mod net_health_policy;
pub mod orchestrator;
pub(crate) mod helper_bootstrap;
pub mod isekai_pipe_quic_transport;
pub mod multipath_transport;
pub(crate) mod rebind_manager;
pub(crate) mod rebind_ports;
pub(crate) mod rebind_driver;
pub mod isekai_stun_p2p_transport;
pub mod isekai_link_relay_transport;
#[cfg(test)]
pub(crate) mod faulty_stream;
pub(crate) mod faulty_udp_socket;
pub mod debug_fault;
pub(crate) mod resume_client;
pub(crate) mod android_quic_endpoint;
pub mod reattach_persistence;

pub use quic_transport::QuicConfig;
pub use orchestrator::{create_session_orchestrator, SessionOrchestrator};

use std::sync::Arc;
use std::sync::LazyLock;
use tokio::runtime::Runtime;
use russh::client;

use crate::session::SessionCore;
use crate::transport::{TransportCommand, TransportEvent, run_ssh_channel_loop};

pub(crate) static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    Runtime::new().expect("Failed to create Tokio runtime")
});

#[cfg(target_os = "android")]
pub(crate) fn init_logger() {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("isekai-terminal-core"),
    );
}

#[cfg(not(target_os = "android"))]
pub(crate) fn init_logger() {}

// ── ターミナル配色テーマ ──────────────────────────────────
// 配色パレット自体（ANSI 16色・デフォルト fg/bg）は `theme` モジュールが
// プロセス全体で共有するグローバル状態として保持する（`theme::Theme` 参照）。
// ここではその差し替え用の UniFFI エントリポイントのみを公開する。

/// ターミナルの配色テーマを差し替える（プロファイル毎ではなくグローバル設定）。
///
/// `ansi16` は SGR が参照する 16 色を ARGB（`0xAARRGGBB`）で `[normal 8色, bright 8色]`
/// の順に渡す。16 個に満たない場合は残りを既定テーマの値で埋め、16 個を超える分は無視する。
/// 呼び出し以降にパースされる SGR にのみ反映され、既に scrollback に積まれた行は
/// 遡って再着色されない（既知の制約）。
#[uniffi::export]
pub fn set_terminal_theme(ansi16: Vec<u32>, default_fg: u32, default_bg: u32) {
    theme::set(theme::from_raw(ansi16, default_fg, default_bg));
}

/// tmux 迂回 control-plane(`ISEKAI_PIPE_DESIGN.md` §8 Epic M)を有効にするか
/// (プロファイル毎ではなくグローバル設定、`set_terminal_theme`と同じ形)。有効な間、
/// 新しく開くSSHチャネル(タブ)は接続直後にリモートへ`streamlocal_forward`を要求し、
/// `isekai-pipe ctl title|clip push`をtmuxを経由せず直接受け取れるようにする。
#[uniffi::export]
pub fn set_ctl_socket_forward_enabled(enabled: bool) {
    transport::set_ctl_socket_forward_enabled(enabled);
}

/// isekai-terminal-core の crate バージョン（`Cargo.toml` の `version`）を返す。
///
/// iOS 対応 Phase 0 の技術検証スパイクで、UniFFI Swift バインディング経由の
/// round-trip（Swift → Rust 呼び出し → 戻り値）を確認するための診断用関数
/// （`PLAN.md` の「Phase Y」節参照）。
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Rust の `async fn` が UniFFI 経由で Swift の `async`/`await` として呼べることを
/// 確認するための診断用関数（Phase 1A-1、iOSアプリ雛形のround-trip検証）。
#[uniffi::export]
pub async fn core_ping() -> String {
    "pong".to_string()
}

/// Phase 1A-1 の診断用 callback interface。UniFFI の `callback_interface` が
/// Swift 側で `protocol` として実装でき、実際に呼び出せることを確認する。
#[uniffi::export(callback_interface)]
pub trait DiagnosticCallback: Send + Sync {
    fn on_diagnostic_event(&self, message: String);
}

/// Phase 1A-1 の診断用 UniFFI Object。Swift 側での生成・明示的な破棄が
/// 正しく動くことを確認する（セッション/接続の状態は一切持たない）。
#[derive(uniffi::Object)]
pub struct DiagnosticHandle;

#[uniffi::export]
impl DiagnosticHandle {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    pub fn fire_callback(&self, callback: Box<dyn DiagnosticCallback>) {
        callback.on_diagnostic_event("hello from Rust".to_string());
    }
}

// ── Phase 1A-4: 連番付きEventQueue（診断用の最小実装） ──────────
//
// 「Swift Actorがcallback到達順序を保証する」という設計は誤りだった
// （複数RustスレッドからのcallbackをそれぞれTask化すると、Actorへ到達する順序が
// 元のcallback発生順である保証はない。Swift Task実行順は決定的FIFOではない。
// ChatGPT外部レビュー2026-07-04、PLAN.md「Phase Y」節参照）。
// 代わりにRust側が単調増加する`sequence`を払い出すSSOTになり、Swift側は
// wake通知を受けてから`drain_events()`で能動的に取得する。ここではその最小骨格を
// 診断用途で実証する（実際のOrchestratorCallbackへの統合はPhase 1Cで行う）。

/// `DiagnosticEventQueue`から取り出す1件のイベント。`sequence`はキュー単位で
/// 単調増加し、Swift側はこの値で「まだ処理していない最古のイベント」を判定する。
#[derive(Debug, Clone, uniffi::Record)]
pub struct DiagnosticEventEnvelope {
    pub sequence: u64,
    pub message: String,
}

/// イベントが追加されたことをSwiftへ知らせるためだけのcallback。
/// 高頻度データ本体はここに載せず、「取りに来てよい」という合図のみを送る。
#[uniffi::export(callback_interface)]
pub trait EventWakeListener: Send + Sync {
    fn events_available(&self);
}

/// 診断用の最小EventQueue。`session_id`/`generation`は持たず`sequence`のみを
/// 発行する（実運用でのSession単位のキューはPhase 1C側で設計する）。
#[derive(uniffi::Object)]
pub struct DiagnosticEventQueue {
    inner: std::sync::Mutex<std::collections::VecDeque<DiagnosticEventEnvelope>>,
    next_sequence: std::sync::atomic::AtomicU64,
    wake_listener: std::sync::Mutex<Option<Box<dyn EventWakeListener>>>,
}

#[uniffi::export]
impl DiagnosticEventQueue {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: std::sync::Mutex::new(std::collections::VecDeque::new()),
            next_sequence: std::sync::atomic::AtomicU64::new(1),
            wake_listener: std::sync::Mutex::new(None),
        })
    }

    /// Swift側の`CallbackIngress`をwake通知の宛先として登録する。
    pub fn set_wake_listener(&self, listener: Box<dyn EventWakeListener>) {
        *self.wake_listener.lock().unwrap() = Some(listener);
    }

    /// イベントをキューへ追加し、登録済みならwake通知を送る。複数スレッドから
    /// 呼ばれてもキュー内の順序は`sequence`の発行順（Mutex経由の直列化）で決まる。
    pub fn push(&self, message: String) {
        let sequence = self
            .next_sequence
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner
            .lock()
            .unwrap()
            .push_back(DiagnosticEventEnvelope { sequence, message });
        if let Some(listener) = self.wake_listener.lock().unwrap().as_ref() {
            listener.events_available();
        }
    }

    /// `after_sequence`より新しいイベントを`sequence`昇順で最大`max_count`件返す。
    /// キューからは取り出さず、返した範囲を先頭から削除する（一度読んだ分だけ捨てる）。
    pub fn drain_events(&self, after_sequence: u64, max_count: u32) -> Vec<DiagnosticEventEnvelope> {
        let mut guard = self.inner.lock().unwrap();
        let mut result = Vec::new();
        while let Some(front) = guard.front() {
            if front.sequence <= after_sequence {
                guard.pop_front();
                continue;
            }
            if result.len() >= max_count as usize {
                break;
            }
            result.push(guard.pop_front().unwrap());
        }
        result
    }
}

// ── Phase 1A-6: Rust→Swift画面更新ブリッジ（診断用の最小実装） ──────────
//
// セルごとにcallbackしない設計。UniFFI境界のデータ形式を具体化し、latest-wins
// （画面Damageは古いものを破棄してよい）というControlEventQueueとは異なる
// 配送ポリシーを実証する（ChatGPT外部レビュー2026-07-04、PLAN.md「Phase Y」節）。
// 実際のVTE(`terminal`モジュール)との統合はPhase 1Bで行う。

/// 1文字分の表示属性。`start`/`length`は`text`のUTF-16コードユニットオフセット。
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct AttributeRun {
    pub start: u32,
    pub length: u32,
    pub fg_argb: u32,
    pub bg_argb: u32,
    pub bold: bool,
    pub underline: bool,
}

/// ターミナル1行分。セルオブジェクトの配列ではなく、UTF-8テキストバッファ+
/// セル幅配列+属性runにまとめることで、全角文字・結合文字・絵文字を
/// 個別セルへ分解せずに扱える。
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PackedRow {
    pub text: String,
    pub cell_widths: Vec<u8>,
    pub attribute_runs: Vec<AttributeRun>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct CursorState {
    pub row: u32,
    pub col: u32,
    pub visible: bool,
}

/// UniFFI境界を渡す画面更新の単位。`screen_generation`はresize等で
/// 不連続に変わる世代番号、`frame_sequence`は同一世代内で単調増加する連番。
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct TerminalFrameBatch {
    pub session_id: String,
    pub screen_generation: u64,
    pub frame_sequence: u64,
    pub rows: Vec<PackedRow>,
    pub dirty_top: u32,
    pub dirty_bottom: u32,
    pub cursor: CursorState,
    pub title: Option<String>,
    pub bell: bool,
}

/// 診断用の最小frame配送ボックス。`DiagnosticEventQueue`と違い全件保持せず、
/// 常に最新の1件だけを保持する（latest-wins）。`screen_generation`が現在保持
/// しているものより古い、または同一世代内で`frame_sequence`が進んでいない
/// frameは黙って破棄する（resize後に古い世代のframeが遅れて届いても
/// 適用しないための仕組み）。
#[derive(uniffi::Object)]
pub struct DiagnosticFrameMailbox {
    latest: std::sync::Mutex<Option<TerminalFrameBatch>>,
    wake_listener: std::sync::Mutex<Option<Box<dyn EventWakeListener>>>,
}

#[uniffi::export]
impl DiagnosticFrameMailbox {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            latest: std::sync::Mutex::new(None),
            wake_listener: std::sync::Mutex::new(None),
        })
    }

    pub fn set_wake_listener(&self, listener: Box<dyn EventWakeListener>) {
        *self.wake_listener.lock().unwrap() = Some(listener);
    }

    /// frameを配送する。古い世代/古い連番のframeは黙って無視する。
    /// 新規に採用された場合のみwake通知を送る。
    pub fn publish(&self, frame: TerminalFrameBatch) {
        let mut guard = self.latest.lock().unwrap();
        if let Some(existing) = guard.as_ref() {
            if frame.screen_generation < existing.screen_generation {
                return;
            }
            if frame.screen_generation == existing.screen_generation
                && frame.frame_sequence <= existing.frame_sequence
            {
                return;
            }
        }
        *guard = Some(frame);
        drop(guard);
        if let Some(listener) = self.wake_listener.lock().unwrap().as_ref() {
            listener.events_available();
        }
    }

    /// 保持している最新frameを取り出す（取り出すと空になる。次の`publish`まで
    /// 同じframeを二重に取得することはない）。
    pub fn take_latest(&self) -> Option<TerminalFrameBatch> {
        self.latest.lock().unwrap().take()
    }
}

// ── ターミナル入力キー変換（Android/iOS共通化） ──────────────────
//
// Android版`TerminalKeyEncoder.kt`(app/src/main/kotlin/tools/isekai/terminal/input/)と
// iOS版`TerminalKeyMapper.swift`がほぼ同一の「キー→制御シーケンス変換」を
// それぞれ独立実装していたため、Rust側へ統合した(2026-07-04、Android/iOS共通化の
// 一環)。純粋関数でセッション/接続状態を持たないためrust-ssot.mdの直接の対象では
// ないが、両OSで内容が重複していたためSSOTを1箇所にまとめる。
//
// Androidの`KeyEvent.keyCode`やiOSの`UIKey`はプラットフォーム固有の値なので、
// 「どの物理/仮想キーが押されたか」の判定は各OS側に残し、変換後の
// `TerminalSpecialKey`（プラットフォーム非依存の列挙）だけをこの関数へ渡す設計にする。

/// プラットフォーム非依存のターミナル特殊キー。F1〜F12と`ForwardDelete`はAndroid版
/// には無かった機能で、この統合を機に追加された(iOS版`TerminalKeyMapper`由来)。
/// `Delete`はAndroidの`KEYCODE_DEL`(実質バックスペース、0x7F)に対応し、iOS版の
/// 前方削除キー(forward delete, `ESC[3~`)とは別物であることに注意。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TerminalSpecialKey {
    Enter,
    Delete,
    ForwardDelete,
    Tab,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    PageUp,
    PageDown,
    Home,
    End,
    FunctionKey { number: u8 },
}

/// xterm互換の修飾キー状態。`terminal_special_key_bytes`へ渡し、矢印・Home/End・
/// PageUp/Down・F1〜F12のシーケンスに修飾子パラメータを付与するために使う
/// (Ctrl+矢印でreadline/tmuxのワード単位移動等を機能させるため、`TERM=xterm-256color`
/// が広告する修飾子付きシーケンスをこちら側でも生成する必要がある)。
/// 全フィールドfalse(修飾なし)は`Default`で表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, uniffi::Record)]
pub struct TerminalKeyModifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub meta: bool,
}

impl TerminalKeyModifiers {
    fn is_none(&self) -> bool {
        !self.shift && !self.alt && !self.ctrl && !self.meta
    }

    /// xterm互換の修飾子パラメータ値: `1 + Shift(1) + Alt(2) + Ctrl(4) + Meta(8)`。
    /// 修飾なしの場合は呼び出し側で`is_none()`により別扱い(このメソッドは呼ばれない)。
    fn xterm_param(&self) -> u8 {
        1 + (self.shift as u8) + (self.alt as u8) * 2 + (self.ctrl as u8) * 4 + (self.meta as u8) * 8
    }
}

/// Kitty keyboard protocol(タスク#54、`ScreenUpdate::kitty_keyboard_flags`)の
/// progressive enhancement flagsのうちbit0。`terminal_special_key_bytes`が参照する
/// (`terminal.rs`のflagsスタックのdocコメントにあるビット割当と一致させること)。
const KITTY_DISAMBIGUATE_ESCAPE_CODES: u16 = 0b1;

/// 特殊キーを、ターミナルへ送信するバイト列(ANSI/xtermエスケープシーケンス)に
/// 変換する。
///
/// - 矢印キーは、修飾子が一切無い場合のみ`application_cursor_mode`(DECCKM)に従い
///   SS3形式(`ESC O A`等)/CSI形式(`ESC[A`等)を切り替える。**修飾子が1つでも
///   付いている場合はDECCKMの値に関わらず常にCSI形式**(`ESC[1;5A`等、xterm互換)
///   になる(DECCKMはSS3/CSIの切替のみを制御し、修飾子パラメータ付きシーケンスは
///   元々CSI形式でしか表現できないため)。
/// - Home/End/PageUp/PageDownも同様に、修飾子が無ければ従来通りの無パラメータ形式
///   (`ESC[H`/`ESC[5~`等)、修飾子があればパラメータ付き(`ESC[1;5H`/`ESC[5;5~`等)。
/// - F1〜F4は修飾子が無ければSS3形式(`ESC O P`等)だが、修飾子が付くと
///   SS3では修飾子パラメータを表現できないため**CSI形式に切り替わる**
///   (`ESC[1;5P`等)。F5〜F12はどちらの場合もCSI `~`形式(修飾子有りなら
///   `ESC[15;5~`等)。未対応のfunction key番号は空配列を返す。
/// - Tabは修飾子無しなら`0x09`だが、Shift単独の場合はCBT(Cursor Backward Tab、
///   `ESC[Z`)を返す(readline/tmux等の「戻りタブ補完」に必要。xterm互換で
///   パラメータは付かない)。Shift以外の修飾子(Ctrl+Tab等)はターミナル制御
///   シーケンスとして標準化されていないため無視し、無修飾のTabとして扱う。
/// - `kitty_flags`(タスク#54で交渉・`ScreenUpdate::kitty_keyboard_flags`として公開される
///   Kitty keyboard protocolのnegotiated flags、呼び出し側はそこから取得した最新値を
///   毎回渡すこと)にbit0(`0b1`、disambiguate escape codes)が立っている場合のみEscapeキーが
///   `ESC[27u`(Kitty `CSI u`形式)になる。Escapeキー(バイト`0x1B`)は本来それ自体が任意の
///   エスケープシーケンスの開始バイトと衝突しうるためこのbitが名指しする典型例
///   (<https://sw.kovidgoyal.net/kitty/keyboard-protocol/#disambiguate>: "pressing the Esc
///   key generates the byte 0x1b which also is used to indicate the start of an escape
///   code")であり、Kitty仕様は無条件で`CSI u`化するよう定めている。矢印・Home/End・
///   PageUp/PageDown・F1〜F12は、同仕様が明示的に許容する代替形式
///   (`CSI 1;<mod>[~ABCDEFHPQS]`)が既存のxterm修飾子CSI形式と完全に一致するため、
///   `kitty_flags`に関わらず上記の挙動をそのまま流用してよい(変更不要)。Enter/Tab/
///   Delete(Backspace相当)/ForwardDeleteも仕様が明示する例外("still generate the same
///   bytes as in legacy mode")でありlegacy形式のまま。Ctrl+英字等の通常テキストキー
///   (`terminal_ctrl_byte`/Unicode文字経路)のCSI u化は本関数のスコープ外(未対応、
///   タスク#72では見送り——`ScreenUpdate::kitty_keyboard_flags`のdocコメント参照)。
#[uniffi::export]
pub fn terminal_special_key_bytes(
    key: TerminalSpecialKey,
    application_cursor_mode: bool,
    modifiers: TerminalKeyModifiers,
    kitty_flags: u16,
) -> Vec<u8> {
    match key {
        TerminalSpecialKey::Enter => vec![0x0D],
        TerminalSpecialKey::Delete => vec![0x7F],
        TerminalSpecialKey::ForwardDelete => b"\x1B[3~".to_vec(),
        TerminalSpecialKey::Tab => terminal_tab_bytes(modifiers),
        TerminalSpecialKey::Escape => {
            if kitty_flags & KITTY_DISAMBIGUATE_ESCAPE_CODES != 0 {
                b"\x1B[27u".to_vec()
            } else {
                vec![0x1B]
            }
        }
        TerminalSpecialKey::ArrowUp => terminal_arrow_bytes(b'A', application_cursor_mode, modifiers),
        TerminalSpecialKey::ArrowDown => terminal_arrow_bytes(b'B', application_cursor_mode, modifiers),
        TerminalSpecialKey::ArrowRight => terminal_arrow_bytes(b'C', application_cursor_mode, modifiers),
        TerminalSpecialKey::ArrowLeft => terminal_arrow_bytes(b'D', application_cursor_mode, modifiers),
        TerminalSpecialKey::PageUp => terminal_tilde_bytes(5, modifiers),
        TerminalSpecialKey::PageDown => terminal_tilde_bytes(6, modifiers),
        TerminalSpecialKey::Home => terminal_home_end_bytes(b'H', modifiers),
        TerminalSpecialKey::End => terminal_home_end_bytes(b'F', modifiers),
        TerminalSpecialKey::FunctionKey { number } => terminal_function_key_bytes(number, modifiers),
    }
}

fn terminal_arrow_bytes(letter: u8, application_cursor_mode: bool, modifiers: TerminalKeyModifiers) -> Vec<u8> {
    if modifiers.is_none() {
        if application_cursor_mode {
            vec![0x1B, 0x4F, letter] // ESC O <letter> (SS3)
        } else {
            vec![0x1B, 0x5B, letter] // ESC [ <letter> (CSI)
        }
    } else {
        terminal_csi_modified(letter, modifiers)
    }
}

fn terminal_home_end_bytes(letter: u8, modifiers: TerminalKeyModifiers) -> Vec<u8> {
    if modifiers.is_none() {
        vec![0x1B, 0x5B, letter] // ESC [ <letter>
    } else {
        terminal_csi_modified(letter, modifiers)
    }
}

/// `ESC [ 1 ; <mod> <letter>`(xterm互換の修飾子付きCSI形式)。
fn terminal_csi_modified(letter: u8, modifiers: TerminalKeyModifiers) -> Vec<u8> {
    let mut out = b"\x1B[1;".to_vec();
    out.extend_from_slice(modifiers.xterm_param().to_string().as_bytes());
    out.push(letter);
    out
}

/// `ESC [ <n> ~`(修飾子無し)、または`ESC [ <n> ; <mod> ~`(修飾子有り)。
fn terminal_tilde_bytes(n: u8, modifiers: TerminalKeyModifiers) -> Vec<u8> {
    if modifiers.is_none() {
        format!("\x1B[{n}~").into_bytes()
    } else {
        format!("\x1B[{n};{}~", modifiers.xterm_param()).into_bytes()
    }
}

fn terminal_tab_bytes(modifiers: TerminalKeyModifiers) -> Vec<u8> {
    if modifiers.shift && !modifiers.ctrl && !modifiers.alt && !modifiers.meta {
        b"\x1B[Z".to_vec() // CBT (Cursor Backward Tab / Shift+Tab)
    } else {
        vec![0x09]
    }
}

fn terminal_function_key_bytes(n: u8, modifiers: TerminalKeyModifiers) -> Vec<u8> {
    match n {
        1..=4 => {
            let letter = b"PQRS"[(n - 1) as usize];
            if modifiers.is_none() {
                vec![0x1B, 0x4F, letter] // ESC O <letter> (SS3)
            } else {
                terminal_csi_modified(letter, modifiers) // SS3では修飾子を表現できないためCSI形式へ切替
            }
        }
        5..=12 => {
            let code: u8 = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => unreachable!(),
            };
            terminal_tilde_bytes(code, modifiers)
        }
        _ => Vec::new(),
    }
}

/// アプリケーションキーパッドモード(DECKPAM/DECKPNM、タスク#43)対応が必要な
/// テンキー(numeric keypad)キー。VT220の物理keypadにある0〜9・`,`・`-`(Subtract)・
/// `.`(Decimal)・Enterに加え、xterm/主要ターミナルエミュレータが同じ`ESC O <letter>`
/// テーブルへ拡張している`+`(Add)・`*`(Multiply)・`/`(Divide)・`=`(Equals)を含む。
/// 左右カッコ(Android `KEYCODE_NUMPAD_LEFT_PAREN`/`KEYCODE_NUMPAD_RIGHT_PAREN`)は
/// このテーブルに存在せず両モードで常に同じリテラル文字を送るため対象外——
/// 呼び出し側は通常のUnicode文字経路([terminal_unicode_char_bytes])にフォール
/// バックすること。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TerminalNumpadKey {
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Decimal,
    Comma,
    Add,
    Subtract,
    Multiply,
    Divide,
    Enter,
    Equals,
}

/// テンキーのバイト列。`application_keypad_mode`(DECKPAM、`Terminal`が`ESC =`/
/// `ESC >`で切り替える、`ScreenUpdate::application_keypad_mode`経由で公開)が
/// `true`ならSS3形式(`ESC O <letter>`、xterm/VT220のapplication keypadテーブルに
/// 準拠)、`false`なら通常のリテラル文字(Enterのみ`0x0D`、`TerminalSpecialKey::Enter`
/// と同じ)を返す。`application_cursor_mode`(#29)と同じ「Rust側は変換ロジックのみ、
/// どのキーコードがどの[TerminalNumpadKey]に対応するかの判定はUI層が行う」という
/// 役割分担。
#[uniffi::export]
pub fn terminal_numpad_key_bytes(key: TerminalNumpadKey, application_keypad_mode: bool) -> Vec<u8> {
    if key == TerminalNumpadKey::Enter {
        return if application_keypad_mode { vec![0x1B, 0x4F, b'M'] } else { vec![0x0D] };
    }
    let (letter, normal): (u8, u8) = match key {
        TerminalNumpadKey::Digit0 => (b'p', b'0'),
        TerminalNumpadKey::Digit1 => (b'q', b'1'),
        TerminalNumpadKey::Digit2 => (b'r', b'2'),
        TerminalNumpadKey::Digit3 => (b's', b'3'),
        TerminalNumpadKey::Digit4 => (b't', b'4'),
        TerminalNumpadKey::Digit5 => (b'u', b'5'),
        TerminalNumpadKey::Digit6 => (b'v', b'6'),
        TerminalNumpadKey::Digit7 => (b'w', b'7'),
        TerminalNumpadKey::Digit8 => (b'x', b'8'),
        TerminalNumpadKey::Digit9 => (b'y', b'9'),
        TerminalNumpadKey::Decimal => (b'n', b'.'),
        TerminalNumpadKey::Comma => (b'l', b','),
        TerminalNumpadKey::Add => (b'k', b'+'),
        TerminalNumpadKey::Subtract => (b'm', b'-'),
        TerminalNumpadKey::Multiply => (b'j', b'*'),
        TerminalNumpadKey::Divide => (b'o', b'/'),
        TerminalNumpadKey::Equals => (b'X', b'='),
        TerminalNumpadKey::Enter => unreachable!("handled above"),
    };
    if application_keypad_mode {
        vec![0x1B, 0x4F, letter]
    } else {
        vec![normal]
    }
}

/// Unicodeコードポイント→バイト列。0(未入力)なら`None`。0x20未満または0x7Fは
/// 単一の制御バイトとして、それ以外はUTF-8としてエンコードする。
/// (Android版`TerminalKeyEncoder.unicodeCharBytes()`のRust移植)
#[uniffi::export]
pub fn terminal_unicode_char_bytes(unicode_char: u32) -> Option<Vec<u8>> {
    if unicode_char == 0 {
        return None;
    }
    if unicode_char < 0x20 || unicode_char == 0x7F {
        Some(vec![unicode_char as u8])
    } else {
        char::from_u32(unicode_char).map(|c| c.to_string().into_bytes())
    }
}

/// トグル式Ctrlキー用: 1コードポイント→Ctrl+<key>の制御コード。変換できない
/// 入力(数字・日本語等)は`None`を返し、呼び出し側は変換せず元の入力をそのまま
/// 送信する。
/// - a-z / A-Z → 0x01-0x1A (Ctrl+A=0x01 ... Ctrl+Z=0x1A)
/// - @ [ \ ] ^ _ (0x40-0x5F) → その5bit下位(Ctrl+@=0x00, Ctrl+[=ESC=0x1B等)
/// - ? (0x3F) → 0x7F (DEL)
/// - スペース(0x20) → 0x00 (NUL)
/// (Android版`TerminalKeyEncoder.ctrlByte()`・iOS版`TerminalKeyMapper.controlByte()`を
/// Rust側へ統合したSSOT実装)
#[uniffi::export]
pub fn terminal_ctrl_byte(code_point: u32) -> Option<u8> {
    if !(0x20..=0x7F).contains(&code_point) {
        return None;
    }
    let Some(ch) = char::from_u32(code_point) else { return None; };
    if ch.is_ascii_alphabetic() {
        return Some((ch.to_ascii_uppercase() as u32 & 0x1F) as u8);
    }
    if (0x40..=0x5F).contains(&code_point) {
        return Some((code_point & 0x1F) as u8);
    }
    match ch {
        '?' => Some(0x7F),
        ' ' => Some(0x00),
        _ => None,
    }
}

/// Kitty keyboard protocol(タスク#54/#72)のbit0(disambiguate escape codes)有効時、
/// Ctrl/Alt(/その組み合わせ・Shift+Alt)付きの印字可能文字キーをCSI u形式
/// (`ESC[<codepoint>;<modifier>u`)へエンコードする(タスク#91)。
///
/// - `code_point`はキーの無修飾時の基本コードポイント(例: Ctrl+AでもAndroid
///   `event.getUnicodeChar(0)`が返す小文字相当の`'a'`)を渡すこと。呼び出し側で
///   大文字/小文字を判定する必要はない(この関数が`to_ascii_lowercase`する)。
/// - `modifier`はxterm/kitty共通のエンコード: `1 + shift(1) + alt(2) + ctrl(4) + meta(8)`。
/// - bit0が立っていない場合、`code_point`が印字可能文字でない場合、修飾キー
///   (Ctrl/Alt)が両方とも押されていない場合は`None`を返す——呼び出し側は
///   `terminal_ctrl_byte`(legacy Ctrl)や"ESCプレフィックス"(legacy Alt)といった
///   既存のフォールバック処理へ進むこと。
/// - Kitty仕様上の例外キー(Enter/Tab/Backspace)は`TerminalSpecialKey`経由の
///   既存分岐が別途処理するためこの関数の対象外(呼び出し側で特殊キー判定を
///   この関数より先に行うこと)。
#[uniffi::export]
pub fn terminal_kitty_disambiguated_key_bytes(
    code_point: u32,
    modifiers: TerminalKeyModifiers,
    kitty_flags: u16,
) -> Option<Vec<u8>> {
    if kitty_flags & KITTY_DISAMBIGUATE_ESCAPE_CODES == 0 {
        return None;
    }
    if !(modifiers.ctrl || modifiers.alt) {
        return None;
    }
    let ch = char::from_u32(code_point)?;
    if !ch.is_ascii_graphic() && ch != ' ' {
        return None;
    }
    let base = ch.to_ascii_lowercase() as u32;
    let mut modifier_value: u32 = 1;
    if modifiers.shift { modifier_value += 1; }
    if modifiers.alt { modifier_value += 2; }
    if modifiers.ctrl { modifier_value += 4; }
    if modifiers.meta { modifier_value += 8; }
    Some(format!("\x1b[{base};{modifier_value}u").into_bytes())
}

/// IME確定テキスト／クリップボードペーストのテキスト→バイト列。改行正規化
/// (`"\r\n"`/`"\n"` → `"\r"`)をここに集約する。複数コードポイントかつ
/// `bracketed_paste_mode`が有効な場合のみ`ESC[200~`...`ESC[201~`で囲む
/// (単一コードポイント、例えば絵文字1文字は囲まない)。
/// (Android版`TerminalKeyEncoder.commitTextBytes()`のRust移植)
#[uniffi::export]
pub fn terminal_commit_text_bytes(text: String, bracketed_paste_mode: bool) -> Vec<u8> {
    if text.is_empty() {
        return Vec::new();
    }
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    let code_point_count = normalized.chars().count();
    if code_point_count > 1 && bracketed_paste_mode {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1B[200~");
        bytes.extend_from_slice(normalized.as_bytes());
        bytes.extend_from_slice(b"\x1B[201~");
        bytes
    } else {
        normalized.into_bytes()
    }
}

// ── 公開型 ──────────────────────────────────────────────

#[derive(Debug, Clone, uniffi::Record)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub cols: u32,
    pub rows: u32,
    /// ローカルポートフォワード(-L)の一覧。接続確立後に自動で待受を開始する。
    pub forwards: Vec<PortForward>,
    /// SSH agent forwarding。既定 OFF・プロファイル単位 opt-in。
    /// 有効でも公開鍵認証以外（パスワード認証）の場合は転送しない。
    /// 有効な場合、サーバー側からの署名要求は毎回ユーザー確認を必須とする
    /// （`OrchestratorCallback::on_agent_sign_request` / `SessionCallback::on_agent_sign_request`）。
    pub agent_forward: bool,
    /// 設定されていれば、`host:port` へ直接ではなく、まずこの踏み台ホストへ
    /// SSH接続・認証し、そこから `channel_open_direct_tcpip` で `host:port` への
    /// チャネルを開いた上にネストしたSSHセッションを張る（`ssh -J` 相当）。
    /// 対象ホストがNAT配下で直接到達できない場合の唯一の到達経路になる。
    pub jump: Option<JumpConfig>,
    /// `forwards` の `bind_address` が非ループバック（127.0.0.0/8・::1・localhost以外）の
    /// 場合に、それを許可するかどうか。既定 false。Kotlin側UI警告だけに頼らずコア側でも
    /// 強制する（Rust SSOTルール、外部レビュー指摘対応）。false時に非ループバックbindが
    /// 指定された場合、そのforwardは`ForwardState::Failed`として拒否される
    /// （セッション自体は切断されない。他のforwardには影響しない）。
    pub allow_non_loopback_forward_bind: bool,
}

/// ProxyJump（多段SSH）の踏み台ホストへの接続情報。`SshConfig::jump` 参照。
#[derive(Debug, Clone, uniffi::Record)]
pub struct JumpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
}

// ── ポートフォワード(-L/-R/-D、Phase 12 P2-2、plain SSHのみ) ─────

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ForwardType {
    /// `ssh -L bind:remote_host:remote_port` 相当。ローカルの`bind_address:bind_port`で
    /// 待受し、接続をSSHサーバー経由で`remote_host:remote_port`へ中継する。
    Local,
    /// `ssh -R bind:remote_host:remote_port` 相当。SSHサーバー側に`bind_address:bind_port`
    /// を listen させ(`tcpip_forward`)、そこへの接続をこちら(クライアント)側から
    /// `remote_host:remote_port`(ローカルのターゲット)へ中継する。`remote_host`/
    /// `remote_port`はLocalと違い「クライアントから見たローカルターゲット」を指す。
    Remote,
    /// `ssh -D bind_port`(SOCKS4/5プロキシ)相当。ローカルの`bind_address:bind_port`で
    /// SOCKSクライアントを受け付け、接続ごとにSOCKSハンドシェイクで宛先を読み取ってから
    /// SSHサーバー経由でそこへ中継する。`remote_host`/`remote_port`は使わない
    /// (宛先は接続ごとに動的に決まるため)。
    Dynamic,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PortForward {
    pub forward_type: ForwardType,
    /// 待受アドレス。Local/Dynamicはクライアント(この端末)側の待受、RemoteはSSHサーバー側の
    /// 待受を指す。既定は "127.0.0.1"("0.0.0.0" 等にすると同一LAN上の第三者から
    /// アクセスされ得るため、`SshConfig.allow_non_loopback_forward_bind`が false の場合は
    /// コア側で拒否される)。
    pub bind_address: String,
    pub bind_port: u16,
    /// Local: 転送先ホスト。Remote: クライアントから見たローカルターゲットのホスト。
    /// Dynamic: 未使用(空文字列でよい、接続ごとにSOCKSハンドシェイクで決まる)。
    pub remote_host: String,
    /// Dynamic: 未使用(0でよい)。
    pub remote_port: u16,
}

/// ポートフォワード待受の状態。`OrchestratorCallback::on_forward_state_changed` で通知される。
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ForwardState {
    Listening,
    Failed { reason: String },
    Stopped,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum SshAuth {
    Password { password: String },
    PublicKey { private_key_pem: Vec<u8> },
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SshError {
    #[error("Connection failed")]
    ConnectionFailed,
    #[error("Authentication failed")]
    AuthFailed,
    #[error("Host key rejected")]
    HostKeyRejected,
    #[error("IO error")]
    IoError,
    #[error("Disconnected")]
    Disconnected,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct CellData {
    pub ch: String,
    pub fg: u32,
    pub bg: u32,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub blink: bool,
    pub invisible: bool,
    /// OSC 8(`ESC]8;params;URIST`、タスク#40)ハイパーリンクのintern id。`Some`なら
    /// `ScreenUpdate::link_table[id]`(0-indexed)にこのセルが指すURLが入っている。
    /// セルごとに`Option<String>`のURLを直接持たせない——`CellData`は`ScreenUpdate`
    /// として毎フレーム全セル分FFIコピーされるため、コストの大きい`String`は
    /// 一度だけ`link_table`に置き、セル側は軽量な`Option<u32>`のみ持つintern方式
    /// にしている(Fableレビュー2次)。
    pub link_id: Option<u32>,
}

/// [SessionOrchestrator::search_scrollback]が返す1件のマッチ位置(タスク#37)。
///
/// - `row`: [SessionOrchestrator::scrollback_cells]と同じ規約——0がライブ画面に
///   一番近い最新のscrollback行、値が大きいほど過去。マッチした行を表示するには
///   そのまま`scrollback_cells(row, ...)`系のoffsetとして使える。
/// - `col`: マッチ開始セルの0-based列。
/// - `len`: マッチが占める表示列数(セル単位)。全角文字を含む場合は文字数より
///   大きくなりうる。
///
/// スコープ外(Fableレビュー2次): scrollbackは折り返しで分割された物理行の
/// `VecDeque`であり、折り返しをまたいだ論理行単位のマッチ(行末と次行先頭に
/// またがる文字列)は検出できない。また、scrollbackは上限(`SCROLLBACK_LIMIT`)を
/// 超えると古い行から追い出されるため、この`row`は呼び出し時点のスナップショットに
/// 対してのみ有効——新しい出力がscrollbackへ積まれる前に使うこと(呼び出し側は
/// `row`を長期キャッシュせず、ジャンプ操作のたびに検索し直す運用を想定する)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ScrollbackSearchMatch {
    pub row: u32,
    pub col: u32,
    pub len: u32,
}

/// OSC 133(タスク#13、セマンティックプロンプト)「前/次のプロンプトへジャンプ」の
/// ジャンプ先。`SessionOrchestrator::jump_to_previous_prompt`/`jump_to_next_prompt`の
/// 結果として`OrchestratorCallback::on_prompt_jump`経由で非同期に届く。
///
/// - `is_live`が`true`の場合、ジャンプ先は現在のライブ画面上にある。呼び出し側は
///   `scrollOffset`を0にリセットし`showingScrollback`をfalseにするだけでよい
///   (`scrollback_cells`を呼ぶ必要はない)。
/// - `is_live`が`false`の場合、`scroll_offset`は[SessionOrchestrator::scrollback_cells]の
///   `offset`引数・[ScrollbackSearchMatch::row]と同じ規約——そのまま`scrollOffset`に
///   代入し`showingScrollback`をtrueにすればよい(タスク#79の「scrollback最新行と
///   ライブ画面表示の`scrollOffset==0`衝突」を`is_live`で明示的に区別する、既存の
///   検索ジャンプと同型のパターン)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct PromptJumpTarget {
    pub scroll_offset: u32,
    pub is_live: bool,
}

/// DECSCUSR(`CSI Ps SP q`)が選択するカーソル形状。`Terminal`が状態として保持し
/// (rust-ssot: Kotlin/Swift側にミラー状態を作らず、この値をそのまま描画に使う)、
/// `ScreenUpdate::cursor_shape`として公開する。点滅の有無は別フィールド
/// (`ScreenUpdate::cursor_blink`)で表現する——DECSET/DECRST `?12`(`CSI ?12h`/
/// `CSI ?12l`、点滅on/offのみを切り替えるレガシー制御、タスク#55)がDECSCUSRとは
/// 独立に同じ`cursor_blink`フィールドを更新できるよう、形状と点滅を分離してある。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

/// DECSET/DECRST `?1000`/`?1002`/`?1003`(タスク#36)が切り替えるマウスレポーティング
/// モード。`Terminal`が状態として保持し(rust-ssot: Kotlin/Swift側にミラー状態を
/// 作らず、この値をそのまま`ScreenUpdate`経由でUI層のジェスチャ裁定に使う——
/// `application_cursor_mode`/`bracketed_paste_mode`と同じ確立済みパターン)、
/// タッチ/ジェスチャイベントをRustへ送るかどうか・どう解釈するかをUI層(#50/#51)が
/// 決める材料にする。実際のエンコード判断(どのイベント種別を報告するか)自体は
/// `Terminal::encode_pointer_event`がこの値を見て行うため、UI層はこの値を
/// 「テキスト選択ジェスチャに倒すかマウスレポートに倒すか」の判断にのみ使えばよい。
///
/// xterm実装に倣い、`?1000`/`?1002`/`?1003`は同一の内部状態を共有する——
/// 複数を続けてset(`h`)した場合は最後にsetしたモードが有効になり、いずれかを
/// reset(`l`)すると番号に関わらずOffへ戻る(`terminal.rs::csi_dispatch`参照)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MouseReportingMode {
    /// マウスレポーティング無効(既定)。
    Off,
    /// `?1000`: ボタンのpress/releaseのみ報告する(移動は報告しない)。
    Normal,
    /// `?1002`: 上記に加え、ボタンを押したままのドラッグ移動も報告する。
    ButtonEvent,
    /// `?1003`: ボタン状態に関係なく全ての移動を報告する(any-event tracking)。
    AnyEvent,
}

/// マウスレポーティング(タスク#36)対象のボタン。左/中/右クリックに加え、
/// モバイルでの主なユースケースであるホイール(縦スクロールジェスチャ)を含める
/// (Fableレビュー指摘: wheelボタン64/65のエンコードを範囲に含める)。
/// 横スクロールホイール(button 6/7)・追加ボタン(button 8以降)は現状使う予定が
/// ないため未対応(必要になったタスクで追加する)。UI層(#50/#51)が生ポインタ
/// イベントを`terminal_pointer_event_bytes`(タスク#51)へ渡す際にも使うため
/// `uniffi::Enum`として公開する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

/// マウスレポーティング(タスク#36)対象のイベント種別。`MouseButton`と同じ理由で
/// `uniffi::Enum`として公開する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MouseEventKind {
    /// ボタン押下(ホイールは常にこの種別で表す — ホイールにはreleaseの概念が無い)。
    Press,
    /// ボタン解放。
    Release,
    /// ポインタ移動。`button`が`Some`ならドラッグ(ボタンを押したまま移動)、
    /// `None`なら単純なホバー移動。
    Motion,
}

/// タスク#51: UI層(Android/iOSのジェスチャハンドラ)が座標付きの生ポインタ
/// イベントを、現在のマウスレポーティング状態に従ってターミナルへ送るべき
/// バイト列にエンコードする。`Terminal::encode_pointer_event`(タスク#36)と
/// 同じロジック(`terminal::encode_pointer_event_bytes`)を、実行中のセッション
/// (`SessionOrchestrator`)を経由せずに直接呼べる純粋関数として公開する
/// (`terminal_special_key_bytes`/`terminal_commit_text_bytes`と同じ設計: UI層は
/// 直近の`ScreenUpdate`から読んだ`mouse_reporting_mode`/`sgr_mouse_mode`/`cols`/
/// `rows`をそのまま引数として渡すだけでよく、「今どのマウスモードか」の判断
/// ロジック自体はここに一元化されたまま——rust-ssot: Kotlin/Swift側に判断ロジックの
/// ミラーを作らない)。
///
/// 報告すべきでないイベント(`mouse_reporting_mode`がOff、またはモードが対象外の
/// イベント種別)は`None`を返す。呼び出し元はこれを「何も送らない」の合図として
/// 扱い、代わりに通常のタッチ処理(テキスト選択・スクロールバックスワイプ等)に
/// フォールバックすればよい。
///
/// `row`/`col`は0-basedのセル座標(画面外の値は端末サイズ`cols`/`rows`へ
/// クランプされる、`terminal::encode_pointer_event_bytes`のdocコメント参照)。
#[uniffi::export]
pub fn terminal_pointer_event_bytes(
    kind: MouseEventKind,
    button: Option<MouseButton>,
    row: u32,
    col: u32,
    modifiers: TerminalKeyModifiers,
    cols: u32,
    rows: u32,
    mouse_reporting_mode: MouseReportingMode,
    sgr_mouse_mode: bool,
) -> Option<Vec<u8>> {
    terminal::encode_pointer_event_bytes(
        terminal::PointerEvent {
            row: row as usize,
            col: col as usize,
            kind,
            button,
            modifiers,
        },
        cols as usize,
        rows as usize,
        mouse_reporting_mode,
        sgr_mouse_mode,
    )
}

/// Sixel(`DCS Pa;Pb;Ph q ... ST`、タスク#42)でデコードされた画像1枚の配置情報。
/// `Terminal`(rust-core)がデコード・配置・寿命管理を一元的に行う(rust-ssot:
/// Android/iOSはこの構造体が指す矩形へ`rgba`をそのままビットマップ描画するだけで
/// よく、「どこに何ピクセルの画像が乗っているか」を判断するロジックをKotlin/Swift
/// 側にミラーしない)。
///
/// `row`/`col`は画像の左上が乗っている`ScreenUpdate.cells`上のセル座標
/// (0-indexed)。`rows_span`/`cols_span`は画像が占めるセル数——実ピクセルサイズ
/// (`width_px`/`height_px`)を、VT340由来の名目セルサイズ(`terminal.rs`の
/// `SIXEL_CELL_WIDTH_PX`/`SIXEL_CELL_HEIGHT_PX`、実フォントのピクセルサイズを
/// このRustコアは知らないため固定値で近似)で割って算出した近似値。呼び出し側は
/// 実際のフォントの`cols_span`×`rows_span`分のセル矩形へ`rgba`(実ピクセルサイズ
/// `width_px`×`height_px`)を引き伸ばして描画すればよい。
///
/// `id`はこの`Terminal`インスタンス内でのみ一意な単調増加id(`u64`が尽きるまで
/// 再利用しない、RIS後もカウンタ自体はリセットしない——過去にキャッシュされた
/// idと衝突させないため)。呼び出し側は前回の`ScreenUpdate.images`との差分を
/// 自前で判断する必要はなく、常に「今回のリストが現在アクティブな画像の全て」
/// として扱い、そのまま描画すればよい(rust-ssot: 消去・スクロールによる立ち退き
/// 等の寿命管理判断はTerminal側で完結しており、UI層は宣言的にリストを反映する
/// だけでよい)。
///
/// スコープ外(実装時点の既知の簡略化、Sixel対応の初版):
/// - 画像は現在の画面(main/alt)全体のスクロール・IL/DL・リサイズ・alt画面切替・
///   全画面消去(ED、`CSI 2J`/`CSI 3J`)のいずれかが起きると無条件に消去される
///   (誤った位置に取り残されるより、消える方が安全側という判断)。部分消去
///   (ED0/ED1、EL、ECH等)では画像は消えない。
/// - Sixel描画によって画面が下端を超えて自動スクロールすることはない(画像は
///   画面下端でクリップされる)。
#[derive(Debug, Clone, uniffi::Record)]
pub struct ImagePlacement {
    pub id: u64,
    pub row: u32,
    pub col: u32,
    pub rows_span: u32,
    pub cols_span: u32,
    pub width_px: u32,
    pub height_px: u32,
    /// RGBA8888、row-major、左上原点。`width_px * height_px * 4`バイト。
    pub rgba: Vec<u8>,
}

/// 1行分の「損傷(damage)」範囲。`line`行目の`left`列から`right`列まで(両端含む)が
/// 前回発行された`ScreenUpdate`から変化したことを表す(タスク#92、Alacrittyの
/// `LineDamageBounds{line,left,right}`に倣った列レンジ差分)。`ScreenUpdate.dirty_rows`
/// が`Some`の時にのみ現れ、UI層(Android/iOS)はこのレンジのセルだけを再描画すればよい。
/// 損傷のない行はリストに含めない(`left <= right`の行のみ)。
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LineDamage {
    pub line: u16,
    pub left: u16,
    pub right: u16,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ScreenUpdate {
    /// 発行するたびに単調増加する連番(0から開始し`wrapping_add(1)`)。UI層への
    /// 配信チャネルが`Channel.CONFLATED`(Android)等でconflateされ、中間の発行が
    /// 読み飛ばされる可能性がある——`dirty_rows`は「直前に発行したScreenUpdateとの
    /// 差分」なので、読み飛ばしが起きると欠落分の変化がdirty_rowsに載らず表示が
    /// 化ける。UI層はこの値が前回受信値+1(wrapping)でなければ読み飛ばしがあったと
    /// 判断し、`dirty_rows`を信用せず全画面再描画にフォールバックすること。
    pub update_seq: u32,
    pub cols: u32,
    pub rows: u32,
    pub cells: Vec<CellData>,
    pub cursor_row: u32,
    pub cursor_col: u32,
    pub title: Option<String>,
    pub application_cursor_mode: bool,
    /// DECKPAM/DECKPNM(`ESC =`/`ESC >`、タスク#43)の現在値。既定は`false`
    /// (numeric keypad mode)。`application_cursor_mode`(#29)と同じ役割分担で、
    /// 実際のテンキーイベントのエンコード(`terminal_numpad_key_bytes`をどう呼ぶか)は
    /// このRustコアではなくUI層のキーエンコーダーが行う。
    pub application_keypad_mode: bool,
    pub bracketed_paste_mode: bool,
    /// DECSET/DECRST `?1000`/`?1002`/`?1003`(タスク#36)の現在値。既定は`Off`。
    /// UI層(#50/#51)はこれを見て、タッチ/ジェスチャイベントをマウスレポートとして
    /// Rustへ送るべきか(＝アプリがマウス報告を要求しているか)を判断できる。
    pub mouse_reporting_mode: MouseReportingMode,
    /// DECSET/DECRST `?1006`(SGR拡張マウスレポーティング、タスク#36)の現在値。
    /// `mouse_reporting_mode`が`Off`でなくても、この値によって
    /// `Terminal::encode_pointer_event`が生成するバイト列の形式(SGR形式か
    /// レガシーX10形式か)が変わる。UI層は直接使わなくてよいが、デバッグ表示や
    /// 将来のプロトコル分岐のために公開しておく。
    pub sgr_mouse_mode: bool,
    /// DECTCEM(`CSI ?25h`/`CSI ?25l`)で制御されるカーソルの表示/非表示。既定は`true`。
    pub cursor_visible: bool,
    /// BEL(0x07)受信のたびに単調増加する世代カウンタ。`bool`ではなくカウンタにして
    /// あるのは、conflated チャネル越しに複数回の BEL が1つの`ScreenUpdate`にまとめ
    /// られても呼び出し側が「前回より進んだか」で取りこぼしを検知でき、かつ同一
    /// `ScreenUpdate`の再適用で二重にフィードバック(バイブ/フラッシュ)が
    /// 発火するのを避けられるため。呼び出し側は前回値と比較し、進んでいれば
    /// フィードバックを1回発火させること。OSC のターミネータとして使われた BEL
    /// (`ESC]0;title BEL`)はカウントされない。
    pub bell_generation: u64,
    /// DECSCUSR(`CSI Ps SP q`)で選択されたカーソル形状。既定は`Block`。
    pub cursor_shape: CursorShape,
    /// カーソルが点滅すべきかどうか。DECSCUSRの偶数/奇数パラメータ
    /// (block/underline/bar それぞれの steady/blinking)から導出される。既定は`true`
    /// (xtermの既定である「blinking block」に合わせる)。
    pub cursor_blink: bool,
    /// OSC 8(タスク#40)ハイパーリンクのURL intern表。`CellData::link_id`はこの
    /// `Vec`のindex(0-indexed)。同一URLは重複排除されて同じindexを指す。
    /// このterminalセッションが一度でも見たURLを(現在アクティブでなくなった後も、
    /// RISされた後も)登録上限(`MAX_LINK_TABLE`、タスク#70)まで保持する——
    /// scrollback上の過去セルの`link_id`がこの表のindexを指し続けるため、
    /// indexを再利用したり表自体をクリアしたりすると過去セルが別のURLを指す
    /// 破損になる(`terminal.rs`の`link_table`フィールドdocコメント参照)。上限
    /// 到達後に見た新規URLはインターンされず、そのURLで開かれたリンクはリンク
    /// 無し扱いにフォールバックする(既存セルの`link_id`参照には影響しない)。
    pub link_table: Vec<String>,
    /// Sixel(タスク#42)で現在アクティブな画像配置の一覧。詳細は[ImagePlacement]参照。
    pub images: Vec<ImagePlacement>,
    /// Kitty keyboard protocol(タスク#54、
    /// <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>)でnegotiateされた
    /// 現在有効なprogressive enhancement flagsのビットマスク。既定は`0`
    /// (legacy mode、拡張無効)。ビットの意味:
    /// `0b00001`=disambiguate escape codes、`0b00010`=report event types
    /// (press/repeat/release)、`0b00100`=report alternate keys(shifted/base
    /// layout)、`0b01000`=report all keys as escape codes、`0b10000`=report
    /// associated text。
    ///
    /// この`Terminal`(rust-core)が担うのはリモートが送ってくる`CSI > flags u`
    /// (push)/`CSI < Pn u`(pop)/`CSI = flags ; mode u`(set)/`CSI ? u`(query、
    /// 応答も自動で行う)を解釈してこの値を保持・公開するところまで(main/alt画面
    /// ごとに独立したflagsスタックを持つ、仕様通りの挙動)。
    ///
    /// 実際のキーイベントのエンコード判断も`application_cursor_mode`(#29)と同じ役割分担
    /// (rust-ssot: 判断ロジックはRust側のSSOT関数に置き、Kotlin/Swiftはこの最新値を
    /// 引数として渡すだけ)——タスク#54実装時点ではこの引数配線が抜けており(タスク#72、
    /// 交渉・公開のみで実際の送信バイト列に無反映というバグ)、修正済み。呼び出し側
    /// (Android`TerminalKeyEncoder.specialKeyBytes`/iOS`TerminalKeyMapper`)は
    /// [terminal_special_key_bytes]へこの値をそのまま渡すこと。現状bit0(disambiguate
    /// escape codes)のEscapeキー(`ESC[27u`化)のみRust側で実装済み——矢印・Home/End・
    /// PageUp/PageDown・F1〜F12は仕様が許容する代替形式が既存のxterm修飾子CSI形式と
    /// 一致するため元々対応不要、Enter/Tab/Backspaceは仕様が明示する例外でlegacyのまま
    /// (詳細は[terminal_special_key_bytes]のdocコメント参照)。bit1〜4(report event
    /// types/alternate keys/all keys as escape codes/associated text)およびCtrl+英字等
    /// 通常テキストキーのCSI u化は未対応(この値の交渉・公開のみ)。
    pub kitty_keyboard_flags: u16,
    /// この`ScreenUpdate`で、前回発行時から実際に変化した行の損傷レンジ一覧
    /// (タスク#92、行単位のdamage tracking)。`None`は「全画面が損傷している=グリッド
    /// 全体を再描画せよ」を意味する(初回発行・寸法変更・スクロール等の構造的変更
    /// [タスク#93]で全画面dirtyになるケース)。`Some(vec)`ならそのレンジのセルのみ
    /// 再描画すればよく、`vec`が空なら(セル内容は前回と同一、`title`等の非グリッド
    /// フィールドだけが変わった等で)グリッドの再描画は不要。カーソル行は下地セルが
    /// 不変でも損傷として含まれる(タスク#94、iOSがカーソルをセル内容と同じ描画パスで
    /// 描くため)。UI層がまだこのフィールドを消費していない段階では、`None`扱いで
    /// 全画面再描画にフォールバックすれば従来通りの挙動になる。
    pub dirty_rows: Option<Vec<LineDamage>>,
}

// ── New orchestrator public types ────────────────────────

/// Phase 7-4: プロファイルが選択するトランスポート戦略。実際のディスパッチは
/// Kotlin 側でこの値に応じて `SessionOrchestrator::connect` /
/// `connect_quic`（tsshd） / `connect_isekai_pipe_quic` / `connect_isekai_pipe_quic_auto`
/// のいずれかを呼び分ける（設定の意図を表す列挙型であり、単一の万能 connect API
/// を意図したものではない。既存の transport ごとに別メソッドを持つ設計を踏襲する）。
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum TransportPreference {
    /// 通常の TCP SSH（Phase 1-4）。
    PlainSsh,
    /// tsshd 互換 QUIC（Phase 5、サーバー側に事前インストールされた tsshd/isekai-helper
    /// 前身を前提とする旧経路）。
    TsshdQuic,
    /// 自作ヘルパー経由 QUIC、フォールバック無し（Phase 7、明示選択時）。
    IsekaiPipeQuic,
    /// 自作ヘルパー経由 QUIC を試し、失敗したら通常の TCP SSH にフォールバックする
    /// （Phase 7、既定推奨）。
    Auto,
    /// 自作ヘルパー経由 QUIC + Tailscale⇔直接アドレスの受動的マルチパスフェイルオーバー
    /// （Phase 9、オプトイン。フォールバック無し）。`direct_host` 未設定なら
    /// `IsekaiPipeQuic` と同等（path0 のみ）。
    IsekaiPipeQuicMultipath,
    /// STUN+SSH rendezvous による直接 P2P QUIC（Phase 10、オプトイン。relay 無し・
    /// 穴あけ不成立時のフォールバック無し）。`isekai_stun_p2p_transport.rs` 参照。
    /// relay 経由の MASQUE ベース P2P（`IsekaiLinkRelayQuic`）とは独立したトランスポート。
    IsekaiStunP2pQuic,
    /// MASQUE relay 経由の P2P QUIC（Phase 10、オプトイン。フォールバック無し）。
    /// `isekai_link_relay_transport.rs` 参照。`IsekaiStunP2pQuic` と異なり relay が常時
    /// 経路に残るため NAT の種類に左右されないが、relay サーバー・JWT が必要。
    IsekaiLinkRelayQuic,
}

/// OSC 52テキストクリップボード(`ClipboardMime::TextPlain`のみ)とtmux迂回チャンネル
/// (`ISEKAI_PIPE_DESIGN.md` §8 Epic M、`isekai_protocol::ClipboardMime`全種)の両方が
/// 運べるmime種別。`isekai_protocol::ClipboardMime`をUniFFI境界越しにそのまま公開できない
/// (isekai-protocolはuniffiに依存しないpure crate)ため、ここに同型を用意する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ClipboardMimeKind {
    TextPlain,
    TextHtml,
    ImagePng,
}

/// クリップボードの中身1件(push時はリモートから受け取った内容、pull時はデバイス側の
/// 現在のクリップボード内容)。`text: String`だった旧シグネチャを置き換える
/// (画像は任意バイト列で運ぶ必要があり、UTF-8前提の`String`では表現できないため)。
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ClipboardPayload {
    pub mime: ClipboardMimeKind,
    pub data: Vec<u8>,
}

/// #10/#22: WiFi/セルラーいずれかに明示的にバインドされたfd。`Network.bindSocket()`
/// (Android)/`IP_BOUND_IF`(iOS、#15)済み・所有権はRust側に移った生fd。
/// `crate::rebind_ports::PlatformFdSource`のUniFFI越しの実体。
#[derive(Debug, Clone, uniffi::Record)]
pub struct PlatformFd {
    pub fd: i32,
    pub local_ip: String,
}

/// #19: 接続失敗の原因をユーザーが自己解決しやすくするための追加ヒント。
/// 判断材料(接続先アドレスの種別等)はRust側(`orchestrator.rs`)に閉じており、
/// Kotlin/Swiftは届いたヒントに応じた案内UIを出すだけでよい(`rust-ssot.md`)。
/// あくまでヒューリスティックなヒントであり、他の理由でも同じ判定になり得る。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ConnectionIssueHint {
    /// 接続先がプライベート/リンクローカルアドレス(ローカルLAN上のホスト)で、
    /// 一度もConnectedに至らないまま切断された。iOSのLocal Network Privacyが
    /// 拒否されていると、こうした接続がサイレントに失敗し続ける。
    LocalNetworkPermissionPossiblyDenied,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ConnectionPublicState {
    Disconnected { reason: Option<String>, issue_hint: Option<ConnectionIssueHint> },
    Connecting,
    Connected { host: String },
    Error { message: String },
    /// 一度`Connected`になったセッションが予期せず切断された際、orchestratorが
    /// 自動的に再接続を試みている間の状態(`orchestrator.rs`のreconnectループ参照)。
    /// `elapsed_secs`/`timeout_secs`はUIがライブなカウントダウンを描画するための
    /// SSOT値(Kotlin側でタイマーを持たない)。
    Reconnecting { elapsed_secs: u32, timeout_secs: u32, reason: Option<String> },
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum TrzszPublicState {
    Idle,
    WaitingUser {
        transfer_id: String,
        mode: String,
        suggested_name: Option<String>,
        expected_size: Option<u64>,
    },
    InProgress {
        transfer_id: String,
        mode: String,
        file_name: Option<String>,
        transferred: u64,
        total: Option<u64>,
    },
    Done {
        transfer_id: String,
        success: bool,
        message: Option<String>,
    },
}

#[uniffi::export(callback_interface)]
pub trait OrchestratorCallback: Send + Sync {
    fn on_connection_state_changed(&self, state: ConnectionPublicState);
    fn on_screen_update(&self, update: ScreenUpdate);
    fn on_host_key(&self, host: String, port: u16, fingerprint: String) -> bool;
    fn on_data(&self, data: Vec<u8>);
    fn on_trzsz_state_changed(&self, state: TrzszPublicState);
    fn on_download_complete(&self, file_name: Option<String>, data: Vec<u8>);
    /// マルチパスtransportで、現在Validatedなpathが1本も無くなった（＝手元のQUIC
    /// コネクション視点で「応答が一切返ってこない」）ことを検知した際に呼ばれる。
    /// キャプティブポータル等はQUICから見ればこれと区別が付かない（100%ロス）ため、
    /// Android OSのキャプティブポータル検知APIより先にこちらで直接検知できる。
    /// マルチパス以外のtransportでは呼ばれない。
    fn on_no_viable_path(&self);
    fn on_forward_state_changed(&self, id: String, state: ForwardState);
    /// SSH agent forwarding: 転送された鍵での署名要求を、要求ごとにユーザーへ確認する。
    /// `true` を返すと署名を実行し、`false` なら拒否する。呼び出し元は host key 確認と
    /// 同じ同期ブロッキング方式（Rust 側の `spawn_blocking` から呼ばれる）を使うため、
    /// この実装は呼び出し元スレッドをブロックしてユーザー操作を待ってよい
    /// （実装例は `TerminalSession.kt` の `onAgentSignRequest` を参照）。
    fn on_agent_sign_request(&self, key_fingerprint: String) -> bool;
    /// リモートが OSC 52 (`ESC]52;c;<base64>BEL`) でクリップボードへの書き込みを要求した
    /// (`ISEKAI_PIPE_DESIGN.md` §8 Epic M)。opt-in設定のチェック・実際にAndroid
    /// `ClipboardManager` へ書くかどうかの判断はKotlin側の責務(単なるイベント通知であり、
    /// セッション/プロトコル状態ではないため`.claude/rules/rust-ssot.md`の対象外)。
    fn on_clipboard_write(&self, payload: ClipboardPayload);
    /// リモートが OSC 52 query(`ESC]52;c;?BEL`)またはtmux迂回チャンネルの
    /// `ClipboardPullRequest`でクリップボードの読み出しを要求した。
    /// `host_key`/`agent_sign_request`確認と同じ同期ブロッキング方式(Rust側の
    /// `spawn_blocking`から呼ばれる)。opt-in設定が無効、またはクリップボードが
    /// 空/取得不可なら`None`を返す(この場合デバイス側からは応答を一切送らない——
    /// 何も返さない方が「機能の有無自体を教えない」という意味で安全なため)。
    /// OSC 52はテキスト専用プロトコルなので、`mime`が`TextPlain`以外の場合にOSC 52へ
    /// 応答するかどうかの判断はRust側(`session.rs`)が行う——Kotlin側は「今デバイスの
    /// クリップボードに何が入っているか」だけを返せばよい。
    fn on_clipboard_pull_request(&self) -> Option<ClipboardPayload>;
    /// #10/#22: `RebindManager`(rebind_manager.rs)がWiFi-bound fdを要求する。
    /// 判断は一切せず、要求された種類のfdを取得して返すだけ(`rust-ssot.md`準拠)。
    /// 取得できなければ`None`(WiFi自体が使えない・権限が無い等)。`host_key`確認等と
    /// 同じ同期ブロッキング方式(Rust側の`spawn_blocking`から呼ばれる)。
    /// マルチパス以外のtransportでは呼ばれない。
    fn on_request_wifi_fd(&self) -> Option<PlatformFd>;
    /// 同、セルラー-bound fd版。
    fn on_request_cellular_fd(&self) -> Option<PlatformFd>;
    /// #19: `RebindManager`の状態が変化した(WiFi/セルラーフェイルオーバー/復帰待ち)。
    /// マルチパス以外のtransportでは呼ばれない。
    fn on_rebind_state_changed(&self, state: crate::rebind_manager::RebindPublicState);
    /// OSC 133(タスク#13)「前/次のプロンプトへジャンプ」(`jump_to_previous_prompt`/
    /// `jump_to_next_prompt`)の結果。ジャンプ先が見つからなければ`None`。
    fn on_prompt_jump(&self, target: Option<PromptJumpTarget>);
    /// OSC 133(タスク#13)「直前コマンドの出力だけをコピー」(`copyLastCommandOutput`)の
    /// 結果。該当コマンドがまだ無ければ`None`。
    fn on_prompt_output_copy_ready(&self, text: Option<String>);
    /// タスク#17(ファイルプレビュー機能): `file_preview_request`で発行した`request_id`の
    /// 結果。`ctl_file.rs`のJSON出力は既にここへ届く前に`FilePreviewOutcome`へ
    /// パース済み(`rust-ssot.md`: JSONパース/base64デコードはRust側で完結させる)。
    fn on_file_preview_result(&self, request_id: String, outcome: crate::file_preview::FilePreviewOutcome);
}

// ── Old callback interface (kept for binary compatibility) ──

pub(crate) trait SessionCallback: Send + Sync {
    fn on_data(&self, data: Vec<u8>);
    fn on_host_key(&self, fingerprint: String) -> bool;
    fn on_connected(&self);
    fn on_disconnected(&self, reason: Option<String>);
    fn on_screen_update(&self, update: ScreenUpdate);
    fn on_trzsz_request(&self, transfer_id: String, mode: String,
                        suggested_name: Option<String>, expected_size: Option<u64>);
    fn on_trzsz_download_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool);
    fn on_trzsz_progress(&self, transfer_id: String, transferred: u64, total: Option<u64>);
    fn on_trzsz_finished(&self, transfer_id: String, success: bool, message: Option<String>);
    fn on_no_viable_path(&self);
    fn on_forward_state_changed(&self, id: String, state: ForwardState);
    fn on_agent_sign_request(&self, key_fingerprint: String) -> bool;
    fn on_clipboard_write(&self, payload: ClipboardPayload);
    fn on_clipboard_pull_request(&self) -> Option<ClipboardPayload>;
    /// #10/#22: デフォルトはNone(マルチパス以外の実装は何もオーバーライドしなくてよい)。
    /// `OrchestratorAdapter`だけが実際に`OrchestratorCallback::on_request_wifi_fd`へ委譲する。
    fn on_request_wifi_fd(&self) -> Option<PlatformFd> { None }
    fn on_request_cellular_fd(&self) -> Option<PlatformFd> { None }
    fn on_rebind_state_changed(&self, _state: crate::rebind_manager::RebindPublicState) {}
    /// タスク#13。デフォルトはno-op(`OrchestratorAdapter`だけが実際に
    /// `OrchestratorCallback::on_prompt_jump`へ委譲する——#10/#22と同じパターン)。
    fn on_prompt_jump(&self, _target: Option<PromptJumpTarget>) {}
    fn on_prompt_output_copy_ready(&self, _text: Option<String>) {}
    /// タスク#17。デフォルトはno-op(`OrchestratorAdapter`だけが実際に
    /// `OrchestratorCallback::on_file_preview_result`へ委譲する——#10/#22と同じパターン)。
    fn on_file_preview_exec_result(&self, _request_id: String, _stdout: Vec<u8>, _exit_status: Option<u32>) {}
}

// ── SshSession ──────────────────────────────────────────
//
// `SessionOrchestrator`(orchestrator.rs)がActiveSession::Sshとして内部的に使う
// 実装。かつてはKotlin/Swiftから`createSshSession`/`SessionCallback`経由で直接
// 使われていたが、両OSともSessionOrchestrator/OrchestratorCallbackへ移行済みのため
// (2026-07-11)、UniFFIへの公開はやめてクレート内部専用にした。

pub(crate) struct SshSession {
    config: SshConfig,
    core: SessionCore,
}

pub(crate) fn create_ssh_session(config: SshConfig) -> Arc<SshSession> {
    init_logger();
    Arc::new(SshSession { config, core: SessionCore::new() })
}

impl SshSession {
    pub(crate) fn connect(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        // config.forwards はコマンドチャネル経由で forward_type に応じたコマンドとして
        // 投入する。run_ssh_channel_loop がシェル起動後に select ループへ入った時点で
        // 消費され、待受タスクが起動する(Kotlin から動的に追加/削除する将来の拡張と
        // 同じ経路)。
        if let Some(tx) = self.core.command_sender() {
            for (i, pf) in config.forwards.iter().enumerate() {
                let id = format!("lf-{i}");
                let cmd = match pf.forward_type {
                    ForwardType::Local => TransportCommand::AddLocalForward {
                        id: id.clone(),
                        bind_addr: pf.bind_address.clone(),
                        bind_port: pf.bind_port,
                        remote_host: pf.remote_host.clone(),
                        remote_port: pf.remote_port,
                    },
                    ForwardType::Remote => TransportCommand::AddRemoteForward {
                        id: id.clone(),
                        bind_addr: pf.bind_address.clone(),
                        bind_port: pf.bind_port,
                        target_host: pf.remote_host.clone(),
                        target_port: pf.remote_port,
                    },
                    ForwardType::Dynamic => TransportCommand::AddDynamicForward {
                        id: id.clone(),
                        bind_addr: pf.bind_address.clone(),
                        bind_port: pf.bind_port,
                    },
                };
                if tx.try_send(cmd).is_err() {
                    log::warn!("ssh: failed to queue initial forward #{i} (id={id}, channel full?)");
                }
            }
        }
        RUNTIME.spawn(async move {
            run_russh_transport(config, cmd_rx, event_tx).await;
        });
        Ok(())
    }

    pub(crate) fn scrollback_len(&self) -> u32 { self.core.scrollback_len() }

    pub(crate) fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        self.core.scrollback_cells(offset, rows)
    }

    pub(crate) fn search_scrollback(&self, query: String, case_sensitive: bool) -> Vec<ScrollbackSearchMatch> {
        self.core.search_scrollback(&query, case_sensitive)
    }

    pub(crate) fn send(&self, data: Vec<u8>) { self.core.send(data); }

    pub(crate) fn resize(&self, cols: u32, rows: u32) { self.core.resize(cols, rows); }

    /// タスク#60: OSのフォーカス変化をそのまま`SessionCore`へ転送する。
    pub(crate) fn notify_focus_change(&self, focused: bool) { self.core.notify_focus_change(focused); }

    /// タスク#13(OSC 133)。
    pub(crate) fn jump_to_previous_prompt(&self, from_scroll_offset: u32, from_showing_scrollback: bool) {
        self.core.jump_to_previous_prompt(from_scroll_offset, from_showing_scrollback);
    }
    pub(crate) fn jump_to_next_prompt(&self, from_scroll_offset: u32, from_showing_scrollback: bool) {
        self.core.jump_to_next_prompt(from_scroll_offset, from_showing_scrollback);
    }
    pub(crate) fn click_to_prompt_cursor(&self, row: u32, col: u32) { self.core.click_to_prompt_cursor(row, col); }
    pub(crate) fn copy_last_command_output(&self) { self.core.copy_last_command_output(); }

    pub(crate) fn disconnect(&self) { self.core.disconnect(); }

    pub(crate) fn trzsz_accept_upload(&self, transfer_id: String, file_name: String,
                               file_size: u64, mode: u32) {
        self.core.trzsz_accept_upload(transfer_id, file_name, file_size, mode);
    }

    pub(crate) fn trzsz_send_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool) {
        self.core.trzsz_send_chunk(transfer_id, data, is_last);
    }

    pub(crate) fn trzsz_accept_download(&self, transfer_id: String) {
        self.core.trzsz_accept_download(transfer_id);
    }

    pub(crate) fn trzsz_cancel(&self, transfer_id: String) {
        self.core.trzsz_cancel(transfer_id);
    }

    pub(crate) fn add_local_forward(
        &self, id: String, bind_address: String, bind_port: u16, remote_host: String, remote_port: u16,
    ) {
        if let Some(tx) = self.core.command_sender() {
            let cmd = TransportCommand::AddLocalForward { id, bind_addr: bind_address, bind_port, remote_host, remote_port };
            if tx.try_send(cmd).is_err() {
                log::warn!("ssh: add_local_forward command dropped (channel full)");
            }
        }
    }

    pub(crate) fn remove_forward(&self, id: String) {
        if let Some(tx) = self.core.command_sender() {
            if tx.try_send(TransportCommand::RemoveForward { id }).is_err() {
                log::warn!("ssh: remove_forward command dropped (channel full)");
            }
        }
    }

    /// タスク#17: ファイルプレビュー用の`isekai-pipe ctl file`execを1本キューイングする。
    /// `command_sender()`が無い(未接続/切断済み)場合は`false`を返し、呼び出し元
    /// (`SessionOrchestrator::file_preview_request`)がその場で`FilePreviewOutcome::Error`を
    /// 合成する。
    pub(crate) fn file_preview_exec(&self, request_id: String, command_line: String) -> bool {
        self.core.file_preview_exec(request_id, command_line)
    }

    /// Phase 12: per-session theme。SessionOrchestrator からのみ呼ばれる内部API。
    pub(crate) fn set_theme(&self, theme: crate::theme::Theme) {
        self.core.set_theme(theme);
    }
}

// ── TCP transport task ───────────────────────────────────

/// SSH接続プーリング(`archive/ISEKAI_SSH_DESIGN.md`参照): 同一ホスト/ユーザー/鍵/
/// `agent_forward`/踏み台へ既に確立済みの認証済み`client::Handle`があれば、新規TCP接続・
/// 新規認証を行わずそれを使い回し、新しいSSHチャネルを1本追加するだけで済ませる。
/// パスワード認証は`pool::SshPoolKey::for_target`が`None`を返すため常にプール対象外
/// (毎回新規接続する、これまでと同じ挙動)。
pub(crate) async fn run_russh_transport(
    mut config: SshConfig,
    cmd_rx: tokio::sync::mpsc::Receiver<TransportCommand>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let russh_config = Arc::new(client::Config {
        keepalive_interval: Some(std::time::Duration::from_secs(60)),
        keepalive_max: 3,
        ..client::Config::default()
    });

    let pool_key = pool::SshPoolKey::for_target(
        &config.host, config.port, &config.username, &config.auth,
        config.agent_forward, &config.jump,
    );

    let pooled = match &pool_key {
        None => {
            match transport::establish_ssh_handle(
                &config.jump, russh_config, &config.host, config.port,
                &config.username, &mut config.auth, config.agent_forward, &event_tx,
            ).await {
                Ok(p) => Arc::new(p),
                Err(msg) => {
                    log::warn!("ssh: {msg}");
                    event_tx.send(TransportEvent::Disconnected { reason: Some(msg) }).await.ok();
                    return;
                }
            }
        }
        Some(key) => match pool::try_attach(&pool::SSH_POOL, key) {
            pool::AttachOutcome::Ready(v) => {
                transport::zeroize_ssh_auth(&mut config.auth);
                v
            }
            pool::AttachOutcome::Waiter(rx) => {
                transport::zeroize_ssh_auth(&mut config.auth);
                match pool::wait_for_establish(rx).await {
                    Ok(v) => v,
                    Err(msg) => {
                        log::warn!("ssh: {msg}");
                        event_tx.send(TransportEvent::Disconnected { reason: Some(msg) }).await.ok();
                        return;
                    }
                }
            }
            pool::AttachOutcome::Establisher => {
                match transport::establish_ssh_handle(
                    &config.jump, russh_config, &config.host, config.port,
                    &config.username, &mut config.auth, config.agent_forward, &event_tx,
                ).await {
                    Ok(p) => pool::publish_success(&pool::SSH_POOL, key, p),
                    Err(msg) => {
                        pool::publish_failure(&pool::SSH_POOL, key, msg.clone());
                        log::warn!("ssh: {msg}");
                        event_tx.send(TransportEvent::Disconnected { reason: Some(msg) }).await.ok();
                        return;
                    }
                }
            }
        },
    };

    run_ssh_channel_loop(
        &pooled, config.cols, config.rows,
        config.agent_forward, config.allow_non_loopback_forward_bind,
        cmd_rx, event_tx,
    ).await;

    if let Some(key) = pool_key {
        pool::release(&pool::SSH_POOL, key, pool::PLAIN_SSH_IDLE_GRACE);
    }
}

#[cfg(test)]
mod diagnostic_event_queue_tests {
    use super::DiagnosticEventQueue;

    #[test]
    fn drain_events_returns_in_sequence_order_and_advances_watermark() {
        let queue = DiagnosticEventQueue::new();
        queue.push("a".to_string());
        queue.push("b".to_string());
        queue.push("c".to_string());

        let first_batch = queue.drain_events(0, 2);
        assert_eq!(first_batch.len(), 2);
        assert_eq!(first_batch[0].sequence, 1);
        assert_eq!(first_batch[0].message, "a");
        assert_eq!(first_batch[1].sequence, 2);
        assert_eq!(first_batch[1].message, "b");

        let last_watermark = first_batch.last().unwrap().sequence;
        let second_batch = queue.drain_events(last_watermark, 10);
        assert_eq!(second_batch.len(), 1);
        assert_eq!(second_batch[0].sequence, 3);
        assert_eq!(second_batch[0].message, "c");

        // 追加のイベントが無ければ空を返す。
        assert!(queue.drain_events(second_batch[0].sequence, 10).is_empty());
    }

    #[test]
    fn drain_events_discards_entries_at_or_below_after_sequence() {
        let queue = DiagnosticEventQueue::new();
        queue.push("a".to_string());
        queue.push("b".to_string());

        // after_sequence が既存の全エントリ以上なら、古い分は破棄されて空が返る
        // （呼び出し側のwatermarkがキューより進んでいる異常系でも取りこぼしを
        // 誤って再配信しないことを確認する）。
        let result = queue.drain_events(100, 10);
        assert!(result.is_empty());
    }

    #[test]
    fn push_wakes_registered_listener() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct CountingListener(Arc<AtomicUsize>);
        impl super::EventWakeListener for CountingListener {
            fn events_available(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let queue = DiagnosticEventQueue::new();
        let count = Arc::new(AtomicUsize::new(0));
        queue.set_wake_listener(Box::new(CountingListener(count.clone())));

        queue.push("x".to_string());
        queue.push("y".to_string());

        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}

#[cfg(test)]
mod diagnostic_frame_mailbox_tests {
    use super::{CursorState, DiagnosticFrameMailbox, TerminalFrameBatch};

    /// どのframeが採用されたかを見分けるためだけに`title`へタグを詰める。
    fn tagged_frame(generation: u64, sequence: u64, tag: &str) -> TerminalFrameBatch {
        TerminalFrameBatch {
            session_id: "test-session".to_string(),
            screen_generation: generation,
            frame_sequence: sequence,
            rows: vec![],
            dirty_top: 0,
            dirty_bottom: 0,
            cursor: CursorState { row: 0, col: 0, visible: true },
            title: Some(tag.to_string()),
            bell: false,
        }
    }

    #[test]
    fn newer_sequence_within_same_generation_replaces_latest() {
        let mailbox = DiagnosticFrameMailbox::new();
        mailbox.publish(tagged_frame(1, 1, "first"));
        mailbox.publish(tagged_frame(1, 2, "second"));

        let latest = mailbox.take_latest().unwrap();
        assert_eq!(latest.title, Some("second".to_string()));
        assert!(mailbox.take_latest().is_none());
    }

    #[test]
    fn stale_sequence_within_same_generation_is_discarded() {
        let mailbox = DiagnosticFrameMailbox::new();
        mailbox.publish(tagged_frame(1, 5, "newer"));
        mailbox.publish(tagged_frame(1, 3, "older-arrived-late"));

        let latest = mailbox.take_latest().unwrap();
        assert_eq!(latest.title, Some("newer".to_string()));
    }

    #[test]
    fn older_generation_is_discarded_even_with_higher_sequence() {
        let mailbox = DiagnosticFrameMailbox::new();
        mailbox.publish(tagged_frame(2, 1, "generation-2"));
        // resize後に古い世代(generation 1)のframeが遅れて届いても、
        // sequenceが大きくても採用してはいけない。
        mailbox.publish(tagged_frame(1, 999, "stale-generation-1"));

        let latest = mailbox.take_latest().unwrap();
        assert_eq!(latest.title, Some("generation-2".to_string()));
    }

    #[test]
    fn newer_generation_replaces_regardless_of_sequence() {
        let mailbox = DiagnosticFrameMailbox::new();
        mailbox.publish(tagged_frame(1, 100, "generation-1"));
        mailbox.publish(tagged_frame(2, 1, "generation-2"));

        let latest = mailbox.take_latest().unwrap();
        assert_eq!(latest.title, Some("generation-2".to_string()));
    }
}

#[cfg(test)]
mod terminal_key_mapping_tests {
    use super::*;

    // Android版TerminalKeyEncoderTest.kt(28件)と対応する形で移植。
    // 「Rust側へ統合しても既存の挙動が一切変わっていない」ことを両言語で
    // 相互検証する意図。

    // 修飾なし(全フィールドfalse)。既存(#29以前)の挙動を検証する回帰テスト群で使う。
    const NO_MODS: TerminalKeyModifiers = TerminalKeyModifiers { shift: false, alt: false, ctrl: false, meta: false };

    #[test]
    fn enter_maps_to_cr() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Enter, false, NO_MODS, 0), vec![0x0D]);
    }

    #[test]
    fn del_maps_to_0x7f() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Delete, false, NO_MODS, 0), vec![0x7F]);
    }

    #[test]
    fn forward_delete_maps_to_csi_tilde() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ForwardDelete, false, NO_MODS, 0), b"\x1B[3~".to_vec());
    }

    // ── Kitty keyboard protocol disambiguate escape codes(タスク#54で交渉のみ実装され
    // 実際のエンコードに未反映だったのをタスク#72で修正) ──────────────────

    #[test]
    fn escape_uses_kitty_csi_u_when_disambiguate_flag_negotiated() {
        // CSI > 1 u でリモートがdisambiguate escape codes(bit0)をpushした状態を想定。
        assert_eq!(
            terminal_special_key_bytes(TerminalSpecialKey::Escape, false, NO_MODS, KITTY_DISAMBIGUATE_ESCAPE_CODES),
            b"\x1B[27u".to_vec()
        );
    }

    #[test]
    fn escape_stays_legacy_byte_when_kitty_flags_do_not_include_disambiguate_bit() {
        // report-event-types(bit1)のみのように、disambiguateビット(bit0)を含まないflagsでは
        // 従来通り生の0x1Bのまま(仕様のdisambiguate専用挙動のため)。
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Escape, false, NO_MODS, 0b10), vec![0x1B]);
    }

    #[test]
    fn kitty_disambiguate_flag_does_not_change_keys_kitty_spec_exempts_or_already_matches() {
        // Kitty仕様: Enter/Tab/Backspace(Delete)は明示的な例外でlegacyのまま。矢印・Home/End・
        // PageUp/PageDown・F1〜F12は仕様が許容する代替形式が既存のxterm修飾子CSI形式と一致する
        // ため、disambiguateビットが立っていても出力は変わらない(関数docコメント参照)。
        let flags = KITTY_DISAMBIGUATE_ESCAPE_CODES;
        let ctrl = TerminalKeyModifiers { ctrl: true, ..Default::default() };
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Enter, false, NO_MODS, flags), vec![0x0D]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Tab, false, NO_MODS, flags), vec![0x09]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Delete, false, NO_MODS, flags), vec![0x7F]);
        assert_eq!(
            terminal_special_key_bytes(TerminalSpecialKey::ArrowUp, false, NO_MODS, flags),
            vec![0x1B, 0x5B, 0x41]
        );
        assert_eq!(
            terminal_special_key_bytes(TerminalSpecialKey::ArrowUp, false, ctrl, flags),
            b"\x1B[1;5A".to_vec()
        );
        assert_eq!(
            terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 1 }, false, NO_MODS, flags),
            b"\x1BOP".to_vec()
        );
    }

    // ── terminal_kitty_disambiguated_key_bytes(タスク#91) ─────────────

    #[test]
    fn kitty_ctrl_letter_uses_csi_u_when_disambiguate_flag_negotiated() {
        let ctrl = TerminalKeyModifiers { ctrl: true, ..Default::default() };
        // Ctrl+A: 'a' = 97, modifier = 1 + ctrl(4) = 5
        assert_eq!(
            terminal_kitty_disambiguated_key_bytes('a' as u32, ctrl, KITTY_DISAMBIGUATE_ESCAPE_CODES),
            Some(b"\x1B[97;5u".to_vec())
        );
    }

    #[test]
    fn kitty_ctrl_letter_lowercases_uppercase_code_point() {
        let ctrl = TerminalKeyModifiers { ctrl: true, ..Default::default() };
        // 呼び出し側が大文字コードポイント('A' = 65)を渡しても、小文字の基本キー(97)へ正規化する。
        assert_eq!(
            terminal_kitty_disambiguated_key_bytes('A' as u32, ctrl, KITTY_DISAMBIGUATE_ESCAPE_CODES),
            Some(b"\x1B[97;5u".to_vec())
        );
    }

    #[test]
    fn kitty_alt_letter_uses_csi_u_when_disambiguate_flag_negotiated() {
        let alt = TerminalKeyModifiers { alt: true, ..Default::default() };
        // Alt+A: modifier = 1 + alt(2) = 3
        assert_eq!(
            terminal_kitty_disambiguated_key_bytes('a' as u32, alt, KITTY_DISAMBIGUATE_ESCAPE_CODES),
            Some(b"\x1B[97;3u".to_vec())
        );
    }

    #[test]
    fn kitty_ctrl_alt_letter_combines_modifier_bits() {
        let ctrl_alt = TerminalKeyModifiers { ctrl: true, alt: true, ..Default::default() };
        // Ctrl+Alt+A: modifier = 1 + alt(2) + ctrl(4) = 7
        assert_eq!(
            terminal_kitty_disambiguated_key_bytes('a' as u32, ctrl_alt, KITTY_DISAMBIGUATE_ESCAPE_CODES),
            Some(b"\x1B[97;7u".to_vec())
        );
    }

    #[test]
    fn kitty_shift_alt_letter_combines_modifier_bits() {
        let shift_alt = TerminalKeyModifiers { shift: true, alt: true, ..Default::default() };
        // Shift+Alt+A: modifier = 1 + shift(1) + alt(2) = 4
        assert_eq!(
            terminal_kitty_disambiguated_key_bytes('a' as u32, shift_alt, KITTY_DISAMBIGUATE_ESCAPE_CODES),
            Some(b"\x1B[97;4u".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguated_key_returns_none_without_disambiguate_bit() {
        let ctrl = TerminalKeyModifiers { ctrl: true, ..Default::default() };
        // report-event-types(bit1)のみのようにdisambiguateビット(bit0)を含まない場合はNone
        // (呼び出し側は既存のlegacy Ctrl/Altエンコードへフォールバックする)。
        assert_eq!(terminal_kitty_disambiguated_key_bytes('a' as u32, ctrl, 0b10), None);
    }

    #[test]
    fn kitty_disambiguated_key_returns_none_without_ctrl_or_alt() {
        // 修飾キーが無ければ通常の印字処理に任せるためNone。
        assert_eq!(
            terminal_kitty_disambiguated_key_bytes('a' as u32, NO_MODS, KITTY_DISAMBIGUATE_ESCAPE_CODES),
            None
        );
    }

    #[test]
    fn kitty_disambiguated_key_returns_none_for_non_printable_code_point() {
        let ctrl = TerminalKeyModifiers { ctrl: true, ..Default::default() };
        // 制御文字(0x01等)や不正なコードポイントは対象外(特殊キー経路等が別途処理する)。
        assert_eq!(terminal_kitty_disambiguated_key_bytes(0x01, ctrl, KITTY_DISAMBIGUATE_ESCAPE_CODES), None);
    }

    #[test]
    fn tab_maps_to_0x09() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Tab, false, NO_MODS, 0), vec![0x09]);
    }

    #[test]
    fn escape_maps_to_0x1b() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Escape, false, NO_MODS, 0), vec![0x1B]);
    }

    #[test]
    fn arrow_keys_map_to_csi() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowUp, false, NO_MODS, 0), vec![0x1B, 0x5B, 0x41]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowDown, false, NO_MODS, 0), vec![0x1B, 0x5B, 0x42]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowRight, false, NO_MODS, 0), vec![0x1B, 0x5B, 0x43]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowLeft, false, NO_MODS, 0), vec![0x1B, 0x5B, 0x44]);
    }

    #[test]
    fn arrow_keys_map_to_ss3_in_application_cursor_mode() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowUp, true, NO_MODS, 0), vec![0x1B, 0x4F, 0x41]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowDown, true, NO_MODS, 0), vec![0x1B, 0x4F, 0x42]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowRight, true, NO_MODS, 0), vec![0x1B, 0x4F, 0x43]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowLeft, true, NO_MODS, 0), vec![0x1B, 0x4F, 0x44]);
    }

    #[test]
    fn page_up_down_and_home_end() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::PageUp, false, NO_MODS, 0), b"\x1B[5~".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::PageDown, false, NO_MODS, 0), b"\x1B[6~".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Home, false, NO_MODS, 0), b"\x1B[H".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::End, false, NO_MODS, 0), b"\x1B[F".to_vec());
    }

    #[test]
    fn function_keys_f1_to_f4_use_ss3() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 1 }, false, NO_MODS, 0), b"\x1BOP".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 4 }, false, NO_MODS, 0), b"\x1BOS".to_vec());
    }

    #[test]
    fn function_keys_f5_to_f12_use_csi_tilde() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 5 }, false, NO_MODS, 0), b"\x1B[15~".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 12 }, false, NO_MODS, 0), b"\x1B[24~".to_vec());
    }

    #[test]
    fn unsupported_function_key_returns_empty() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 99 }, false, NO_MODS, 0), Vec::<u8>::new());
    }

    // ── 修飾キー付きシーケンス(#29) ──────────────────────────

    #[test]
    fn xterm_param_matches_1_plus_shift_alt_ctrl_meta_bitmask() {
        assert_eq!(TerminalKeyModifiers { shift: true, ..Default::default() }.xterm_param(), 2);
        assert_eq!(TerminalKeyModifiers { alt: true, ..Default::default() }.xterm_param(), 3);
        assert_eq!(TerminalKeyModifiers { ctrl: true, ..Default::default() }.xterm_param(), 5);
        assert_eq!(TerminalKeyModifiers { meta: true, ..Default::default() }.xterm_param(), 9);
        assert_eq!(
            TerminalKeyModifiers { shift: true, ctrl: true, ..Default::default() }.xterm_param(),
            6
        );
        assert_eq!(
            TerminalKeyModifiers { shift: true, alt: true, ctrl: true, meta: true }.xterm_param(),
            16
        );
    }

    #[test]
    fn arrow_keys_with_modifiers_always_use_csi_form_regardless_of_decckm() {
        let ctrl = TerminalKeyModifiers { ctrl: true, ..Default::default() };
        // DECCKM無効時
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowUp, false, ctrl, 0), b"\x1B[1;5A".to_vec());
        // DECCKM有効時でも修飾子付きはSS3にならずCSI形式のまま
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowUp, true, ctrl, 0), b"\x1B[1;5A".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowDown, true, ctrl, 0), b"\x1B[1;5B".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowRight, true, ctrl, 0), b"\x1B[1;5C".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowLeft, true, ctrl, 0), b"\x1B[1;5D".to_vec());
    }

    #[test]
    fn arrow_key_shift_uses_modifier_2() {
        let shift = TerminalKeyModifiers { shift: true, ..Default::default() };
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowUp, false, shift, 0), b"\x1B[1;2A".to_vec());
    }

    #[test]
    fn home_end_with_modifiers_use_parameterized_csi() {
        let ctrl = TerminalKeyModifiers { ctrl: true, ..Default::default() };
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Home, false, ctrl, 0), b"\x1B[1;5H".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::End, false, ctrl, 0), b"\x1B[1;5F".to_vec());
    }

    #[test]
    fn page_up_down_with_modifiers_use_parameterized_tilde() {
        let ctrl = TerminalKeyModifiers { ctrl: true, ..Default::default() };
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::PageUp, false, ctrl, 0), b"\x1B[5;5~".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::PageDown, false, ctrl, 0), b"\x1B[6;5~".to_vec());
    }

    #[test]
    fn function_keys_f1_to_f4_switch_from_ss3_to_csi_when_modified() {
        let ctrl = TerminalKeyModifiers { ctrl: true, ..Default::default() };
        assert_eq!(
            terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 1 }, false, ctrl, 0),
            b"\x1B[1;5P".to_vec()
        );
        assert_eq!(
            terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 4 }, false, ctrl, 0),
            b"\x1B[1;5S".to_vec()
        );
    }

    #[test]
    fn function_keys_f5_to_f12_use_parameterized_tilde_when_modified() {
        let ctrl = TerminalKeyModifiers { ctrl: true, ..Default::default() };
        assert_eq!(
            terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 5 }, false, ctrl, 0),
            b"\x1B[15;5~".to_vec()
        );
        assert_eq!(
            terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 12 }, false, ctrl, 0),
            b"\x1B[24;5~".to_vec()
        );
    }

    #[test]
    fn shift_tab_maps_to_cbt() {
        let shift = TerminalKeyModifiers { shift: true, ..Default::default() };
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Tab, false, shift, 0), b"\x1B[Z".to_vec());
    }

    #[test]
    fn tab_with_non_shift_modifiers_falls_back_to_plain_tab() {
        let ctrl = TerminalKeyModifiers { ctrl: true, ..Default::default() };
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Tab, false, ctrl, 0), vec![0x09]);
        let shift_ctrl = TerminalKeyModifiers { shift: true, ctrl: true, ..Default::default() };
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Tab, false, shift_ctrl, 0), vec![0x09]);
    }

    #[test]
    fn keys_unaffected_by_modifiers_stay_the_same() {
        let ctrl = TerminalKeyModifiers { ctrl: true, ..Default::default() };
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Enter, false, ctrl, 0), vec![0x0D]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Delete, false, ctrl, 0), vec![0x7F]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Escape, false, ctrl, 0), vec![0x1B]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ForwardDelete, false, ctrl, 0), b"\x1B[3~".to_vec());
    }

    // ── アプリケーションキーパッドモード(DECKPAM/DECKPNM、タスク#43) ──────────

    #[test]
    fn numpad_digits_are_literal_in_numeric_mode() {
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Digit0, false), b"0".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Digit9, false), b"9".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Decimal, false), b".".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Comma, false), b",".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Add, false), b"+".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Subtract, false), b"-".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Multiply, false), b"*".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Divide, false), b"/".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Equals, false), b"=".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Enter, false), vec![0x0D]);
    }

    #[test]
    fn numpad_digits_map_to_ss3_in_application_keypad_mode() {
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Digit0, true), b"\x1BOp".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Digit1, true), b"\x1BOq".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Digit9, true), b"\x1BOy".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Decimal, true), b"\x1BOn".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Comma, true), b"\x1BOl".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Add, true), b"\x1BOk".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Subtract, true), b"\x1BOm".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Multiply, true), b"\x1BOj".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Divide, true), b"\x1BOo".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Equals, true), b"\x1BOX".to_vec());
        assert_eq!(terminal_numpad_key_bytes(TerminalNumpadKey::Enter, true), b"\x1BOM".to_vec());
    }

    #[test]
    fn zero_codepoint_returns_none() {
        assert_eq!(terminal_unicode_char_bytes(0), None);
    }

    #[test]
    fn control_char_stays_as_single_byte() {
        assert_eq!(terminal_unicode_char_bytes(0x03), Some(vec![0x03]));
    }

    #[test]
    fn ascii_letter_encodes_as_utf8() {
        assert_eq!(terminal_unicode_char_bytes('a' as u32), Some(b"a".to_vec()));
    }

    #[test]
    fn japanese_char_encodes_as_utf8() {
        assert_eq!(terminal_unicode_char_bytes('あ' as u32), Some("あ".as_bytes().to_vec()));
    }

    #[test]
    fn empty_string_returns_empty_bytes() {
        assert_eq!(terminal_commit_text_bytes(String::new(), false), Vec::<u8>::new());
    }

    #[test]
    fn single_char_encodes_as_plain_utf8() {
        assert_eq!(terminal_commit_text_bytes("a".to_string(), false), b"a".to_vec());
    }

    #[test]
    fn multi_char_without_bracketed_paste_mode_encodes_as_plain_utf8() {
        assert_eq!(terminal_commit_text_bytes("ab".to_string(), false), b"ab".to_vec());
    }

    #[test]
    fn multi_char_wraps_in_bracketed_paste_when_enabled() {
        let bytes = terminal_commit_text_bytes("ab".to_string(), true);
        assert_eq!(bytes[0], 0x1B);
        assert!(String::from_utf8_lossy(&bytes).contains("ab"));
        assert_eq!(*bytes.last().unwrap(), 0x7E);
    }

    #[test]
    fn emoji_single_codepoint_does_not_wrap_even_with_bracketed_paste_mode() {
        let emoji = "😀".to_string();
        assert_eq!(terminal_commit_text_bytes(emoji.clone(), true), emoji.into_bytes());
    }

    #[test]
    fn lowercase_and_uppercase_a_map_to_0x01() {
        assert_eq!(terminal_ctrl_byte('a' as u32), Some(0x01));
        assert_eq!(terminal_ctrl_byte('A' as u32), Some(0x01));
    }

    #[test]
    fn lowercase_z_maps_to_0x1a() {
        assert_eq!(terminal_ctrl_byte('z' as u32), Some(0x1A));
    }

    #[test]
    fn at_sign_maps_to_0x00() {
        assert_eq!(terminal_ctrl_byte('@' as u32), Some(0x00));
    }

    #[test]
    fn open_bracket_maps_to_esc() {
        assert_eq!(terminal_ctrl_byte('[' as u32), Some(0x1B));
    }

    #[test]
    fn question_mark_maps_to_del() {
        assert_eq!(terminal_ctrl_byte('?' as u32), Some(0x7F));
    }

    #[test]
    fn space_maps_to_nul() {
        assert_eq!(terminal_ctrl_byte(' ' as u32), Some(0x00));
    }

    #[test]
    fn digit_and_japanese_return_none() {
        assert_eq!(terminal_ctrl_byte('1' as u32), None);
        assert_eq!(terminal_ctrl_byte('あ' as u32), None);
    }

    #[test]
    fn bare_lf_is_normalized_to_cr() {
        assert_eq!(terminal_commit_text_bytes("a\nb".to_string(), false), "a\rb".as_bytes().to_vec());
    }

    #[test]
    fn crlf_is_normalized_to_single_cr() {
        assert_eq!(terminal_commit_text_bytes("a\r\nb".to_string(), false), "a\rb".as_bytes().to_vec());
    }

    #[test]
    fn multiple_lines_all_normalized() {
        assert_eq!(
            terminal_commit_text_bytes("line1\r\nline2\nline3".to_string(), false),
            "line1\rline2\rline3".as_bytes().to_vec()
        );
    }

    #[test]
    fn newline_normalization_happens_before_bracketed_paste_wrapping() {
        let bytes = terminal_commit_text_bytes("a\r\nb".to_string(), true);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("a\rb"));
        assert!(!text.contains("\r\n"));
        assert_eq!(bytes[0], 0x1B);
        assert_eq!(*bytes.last().unwrap(), 0x7E);
    }
}

/// タスク#51: `terminal_pointer_event_bytes`(UI層がRustセッションを経由せず直接
/// 呼べる、生ポインタイベント→バイト列のエンコード関数)の単体テスト。実際の
/// エンコードロジック自体(モード別の報告可否・SGR/レガシー形式・クランプ)は
/// `terminal.rs`の`encode_pointer_event_bytes`テスト群で既にカバー済みのため、
/// ここでは「引数がそのまま委譲先へ届いているか」の配線だけを検証する。
#[cfg(test)]
mod terminal_pointer_event_bytes_tests {
    use super::*;

    const NO_MODS: TerminalKeyModifiers = TerminalKeyModifiers { shift: false, alt: false, ctrl: false, meta: false };

    #[test]
    fn off_mode_reports_nothing() {
        assert_eq!(
            terminal_pointer_event_bytes(
                MouseEventKind::Press, Some(MouseButton::Left), 0, 0, NO_MODS,
                80, 24, MouseReportingMode::Off, false,
            ),
            None
        );
    }

    #[test]
    fn sgr_press_matches_terminal_encode_pointer_event() {
        let bytes = terminal_pointer_event_bytes(
            MouseEventKind::Press, Some(MouseButton::Left), 4, 9, NO_MODS,
            80, 24, MouseReportingMode::Normal, true,
        );
        assert_eq!(bytes, Some(b"\x1b[<0;10;5M".to_vec()));
    }

    #[test]
    fn legacy_x10_release_always_reports_no_button() {
        let bytes = terminal_pointer_event_bytes(
            MouseEventKind::Release, Some(MouseButton::Left), 4, 9, NO_MODS,
            80, 24, MouseReportingMode::Normal, false,
        );
        assert_eq!(bytes, Some(vec![0x1B, b'[', b'M', 32 + 3, 32 + 10, 32 + 5]));
    }

    #[test]
    fn out_of_bounds_coordinates_clamp_to_terminal_size() {
        let bytes = terminal_pointer_event_bytes(
            MouseEventKind::Press, Some(MouseButton::Left), 1000, 1000, NO_MODS,
            80, 24, MouseReportingMode::Normal, true,
        );
        assert_eq!(bytes, Some(b"\x1b[<0;80;24M".to_vec()));
    }

    #[test]
    fn motion_without_button_is_suppressed_in_button_event_mode() {
        assert_eq!(
            terminal_pointer_event_bytes(
                MouseEventKind::Motion, None, 1, 1, NO_MODS,
                80, 24, MouseReportingMode::ButtonEvent, true,
            ),
            None
        );
    }

    #[test]
    fn motion_without_button_is_reported_in_any_event_mode() {
        let bytes = terminal_pointer_event_bytes(
            MouseEventKind::Motion, None, 2, 2, NO_MODS,
            80, 24, MouseReportingMode::AnyEvent, true,
        );
        assert_eq!(bytes, Some(b"\x1b[<35;3;3M".to_vec()));
    }
}
