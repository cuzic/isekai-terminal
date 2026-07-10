uniffi::setup_scaffolding!("isekai_terminal_core");

pub mod trzsz;
pub mod quic_transport;
pub(crate) mod agent_forward;
pub(crate) mod terminal;
pub(crate) mod theme;
pub(crate) mod transport;
pub(crate) mod pool;
pub(crate) mod socks;
pub(crate) mod session_state;
pub(crate) mod session;
pub mod orchestrator;
pub mod session_supervisor;
pub(crate) mod helper_bootstrap;
pub mod isekai_pipe_quic_transport;
pub mod multipath_transport;
pub mod isekai_stun_p2p_transport;
pub mod isekai_link_relay_transport;
#[cfg(test)]
pub(crate) mod faulty_stream;
pub(crate) mod faulty_udp_socket;
pub mod debug_fault;
pub(crate) mod resume_client;
pub(crate) mod android_quic_endpoint;

pub use quic_transport::{create_quic_session, QuicConfig, QuicSession};
pub use orchestrator::{create_session_orchestrator, SessionOrchestrator};
pub use session_supervisor::{create_session_supervisor, ExecutionMode, SessionState, SessionSupervisor};

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

/// 特殊キーを、ターミナルへ送信するバイト列(ANSI/xtermエスケープシーケンス)に
/// 変換する。矢印キーは`application_cursor_mode`が有効ならSS3形式(`ESC O A`等、
/// DECCKM)、無効ならCSI形式(`ESC[A`等)を返す。F1〜F4はSS3形式、F5〜F12は
/// CSI `~`形式(xterm互換)。未対応のfunction key番号は空配列を返す。
#[uniffi::export]
pub fn terminal_special_key_bytes(key: TerminalSpecialKey, application_cursor_mode: bool) -> Vec<u8> {
    match key {
        TerminalSpecialKey::Enter => vec![0x0D],
        TerminalSpecialKey::Delete => vec![0x7F],
        TerminalSpecialKey::ForwardDelete => b"\x1B[3~".to_vec(),
        TerminalSpecialKey::Tab => vec![0x09],
        TerminalSpecialKey::Escape => vec![0x1B],
        TerminalSpecialKey::ArrowUp => terminal_arrow_bytes(b'A', application_cursor_mode),
        TerminalSpecialKey::ArrowDown => terminal_arrow_bytes(b'B', application_cursor_mode),
        TerminalSpecialKey::ArrowRight => terminal_arrow_bytes(b'C', application_cursor_mode),
        TerminalSpecialKey::ArrowLeft => terminal_arrow_bytes(b'D', application_cursor_mode),
        TerminalSpecialKey::PageUp => b"\x1B[5~".to_vec(),
        TerminalSpecialKey::PageDown => b"\x1B[6~".to_vec(),
        TerminalSpecialKey::Home => b"\x1B[H".to_vec(),
        TerminalSpecialKey::End => b"\x1B[F".to_vec(),
        TerminalSpecialKey::FunctionKey { number } => terminal_function_key_bytes(number),
    }
}

fn terminal_arrow_bytes(letter: u8, application_cursor_mode: bool) -> Vec<u8> {
    if application_cursor_mode {
        vec![0x1B, 0x4F, letter] // ESC O <letter> (SS3)
    } else {
        vec![0x1B, 0x5B, letter] // ESC [ <letter> (CSI)
    }
}

fn terminal_function_key_bytes(n: u8) -> Vec<u8> {
    match n {
        1 => b"\x1BOP".to_vec(),
        2 => b"\x1BOQ".to_vec(),
        3 => b"\x1BOR".to_vec(),
        4 => b"\x1BOS".to_vec(),
        5 => b"\x1B[15~".to_vec(),
        6 => b"\x1B[17~".to_vec(),
        7 => b"\x1B[18~".to_vec(),
        8 => b"\x1B[19~".to_vec(),
        9 => b"\x1B[20~".to_vec(),
        10 => b"\x1B[21~".to_vec(),
        11 => b"\x1B[23~".to_vec(),
        12 => b"\x1B[24~".to_vec(),
        _ => Vec::new(),
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
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ScreenUpdate {
    pub cols: u32,
    pub rows: u32,
    pub cells: Vec<CellData>,
    pub cursor_row: u32,
    pub cursor_col: u32,
    pub title: Option<String>,
    pub application_cursor_mode: bool,
    pub bracketed_paste_mode: bool,
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

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ConnectionPublicState {
    Disconnected { reason: Option<String> },
    Connecting,
    Connected { host: String },
    Error { message: String },
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
}

// ── Old callback interface (kept for binary compatibility) ──

#[uniffi::export(callback_interface)]
pub trait SessionCallback: Send + Sync {
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
}

// ── SshSession ──────────────────────────────────────────

#[derive(uniffi::Object)]
pub struct SshSession {
    config: SshConfig,
    core: SessionCore,
}

#[uniffi::export]
pub fn create_ssh_session(config: SshConfig) -> Arc<SshSession> {
    init_logger();
    Arc::new(SshSession { config, core: SessionCore::new() })
}

#[uniffi::export]
impl SshSession {
    pub fn connect(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
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

    pub fn scrollback_len(&self) -> u32 { self.core.scrollback_len() }

    pub fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        self.core.scrollback_cells(offset, rows)
    }

    pub fn send(&self, data: Vec<u8>) { self.core.send(data); }

    pub fn resize(&self, cols: u32, rows: u32) { self.core.resize(cols, rows); }

    pub fn disconnect(&self) { self.core.disconnect(); }

    pub fn trzsz_accept_upload(&self, transfer_id: String, file_name: String,
                               file_size: u64, mode: u32) {
        self.core.trzsz_accept_upload(transfer_id, file_name, file_size, mode);
    }

    pub fn trzsz_send_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool) {
        self.core.trzsz_send_chunk(transfer_id, data, is_last);
    }

    pub fn trzsz_accept_download(&self, transfer_id: String) {
        self.core.trzsz_accept_download(transfer_id);
    }

    pub fn trzsz_cancel(&self, transfer_id: String) {
        self.core.trzsz_cancel(transfer_id);
    }

    /// Phase 1C(#26): OSからネットワーク断を通知された時の対応(`SessionCore`が
    /// 判断、詳細は`session.rs`の`should_abort_on_network_lost`参照)。プレーンSSH
    /// (TCP)は`is_quic=false`固定 — 接続済みでも切断扱いにする。
    pub fn notify_network_lost(&self) {
        self.core.notify_network_lost(false);
    }
}

// ── ポートフォワードの動的追加/削除 ───────────────────────
// SessionOrchestrator からのみ呼ばれる内部 API(uniffi には直接は出さない)。
// MVP の ProfileEditScreen は接続時に forwards をまとめて適用するだけだが、
// 将来 Kotlin から接続中に動的に追加/削除する UI を足すときはここを export すればよい。
impl SshSession {
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

    #[test]
    fn enter_maps_to_cr() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Enter, false), vec![0x0D]);
    }

    #[test]
    fn del_maps_to_0x7f() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Delete, false), vec![0x7F]);
    }

    #[test]
    fn forward_delete_maps_to_csi_tilde() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ForwardDelete, false), b"\x1B[3~".to_vec());
    }

    #[test]
    fn tab_maps_to_0x09() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Tab, false), vec![0x09]);
    }

    #[test]
    fn escape_maps_to_0x1b() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Escape, false), vec![0x1B]);
    }

    #[test]
    fn arrow_keys_map_to_csi() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowUp, false), vec![0x1B, 0x5B, 0x41]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowDown, false), vec![0x1B, 0x5B, 0x42]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowRight, false), vec![0x1B, 0x5B, 0x43]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowLeft, false), vec![0x1B, 0x5B, 0x44]);
    }

    #[test]
    fn arrow_keys_map_to_ss3_in_application_cursor_mode() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowUp, true), vec![0x1B, 0x4F, 0x41]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowDown, true), vec![0x1B, 0x4F, 0x42]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowRight, true), vec![0x1B, 0x4F, 0x43]);
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::ArrowLeft, true), vec![0x1B, 0x4F, 0x44]);
    }

    #[test]
    fn page_up_down_and_home_end() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::PageUp, false), b"\x1B[5~".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::PageDown, false), b"\x1B[6~".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::Home, false), b"\x1B[H".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::End, false), b"\x1B[F".to_vec());
    }

    #[test]
    fn function_keys_f1_to_f4_use_ss3() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 1 }, false), b"\x1BOP".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 4 }, false), b"\x1BOS".to_vec());
    }

    #[test]
    fn function_keys_f5_to_f12_use_csi_tilde() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 5 }, false), b"\x1B[15~".to_vec());
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 12 }, false), b"\x1B[24~".to_vec());
    }

    #[test]
    fn unsupported_function_key_returns_empty() {
        assert_eq!(terminal_special_key_bytes(TerminalSpecialKey::FunctionKey { number: 99 }, false), Vec::<u8>::new());
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
