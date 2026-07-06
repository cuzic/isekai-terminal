//! Phase 7-3: SSH 経由で isekai-helper をリモートへ自動配布・起動するブートストラップロジック。
//!
//! 契約の詳細は `/HELPER_PROTOCOL.md` を参照。ここでは既に認証済みの `russh::client::Handle`
//! 上で、以下を行う:
//!
//! 1. 既存インストール確認（`~/.local/bin/isekai-helper` の存在・バージョン確認）
//! 2. 無ければ（またはバージョン不一致なら）対応アーキテクチャのバイナリを転送
//! 3. 起動し、標準出力からハンドシェイク JSON を受け取る
//!
//! 転送は SFTP ではなく `base64 -d > file` 形式の exec で行う（sshd 側の SFTP subsystem
//! 設定に依存しないため、より広い環境で動く）。

use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine as _;
use isekai_protocol::handshake::decode_handshake_json;
use log::{info, warn};
use russh::{client, ChannelMsg};
use sha2::{Digest, Sha256};

use crate::transport::RusshEventHandler;

// `HELPER_INSTALL_DIR`/`HELPER_BIN_NAME`/`HANDSHAKE_POLL_ATTEMPTS`/
// `HANDSHAKE_POLL_INTERVAL_MS` は `isekai_protocol::bootstrap` で共有している
// （`isekai-bootstrap::openssh` 側の同名定数と実体を一致させるため — 詳細は
// そちらのモジュールdoc参照）。`shell_single_quote`/`validate_relay_sni`/
// `validate_relay_jwt`も同モジュール由来（セキュリティレビュー #57、同じ理由で
// `isekai-bootstrap::openssh`と共有する）。
use isekai_protocol::bootstrap::{
    shell_single_quote, validate_relay_jwt, validate_relay_sni, HANDSHAKE_POLL_ATTEMPTS,
    HANDSHAKE_POLL_INTERVAL_MS, HELPER_BIN_NAME, HELPER_INSTALL_DIR,
};

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("ssh exec failed: {0}")]
    Exec(String),
    #[error("unsupported remote architecture: {0}")]
    UnsupportedArch(String),
    #[error("failed to upload isekai-helper binary: {0}")]
    Upload(String),
    #[error("failed to launch isekai-helper: {0}")]
    Launch(String),
    #[error("handshake not received within timeout")]
    HandshakeTimeout,
    #[error("failed to parse handshake JSON: {0}")]
    HandshakeParse(String),
    /// 固定ポート(`--bind`)でのUDP bindが既に使用中で失敗した(同一サーバーへの
    /// 別セッション/別プロセスが同じポートを握っている可能性が高い)。
    #[error("isekai-helper failed to bind UDP port {0}: already in use (another session on this server may already be using it)")]
    BindPortInUse(u16),
    /// 固定ポートでのUDP bindが権限不足で失敗した(1024未満のポートは通常サーバー側の
    /// 管理者権限が必要)。
    #[error("isekai-helper failed to bind UDP port {0}: permission denied (ports below 1024 usually require elevated privileges on the server)")]
    BindPermissionDenied(u16),
    /// 固定ポートでのUDP bindがこのホストでは使えないアドレスのため失敗した。
    #[error("isekai-helper failed to bind UDP port {0}: address not available on this host")]
    BindAddressUnavailable(u16),
    /// `relay_sni`/`relay_jwt`が厳格な許容文字集合を満たさない(セキュリティレビュー
    /// #57)。シェルクォート自体は常に安全だが、発行元(relay/JWTサーバー)が侵害・
    /// 誤設定された場合の多層防御としてここで弾く。
    #[error("invalid relay parameter: {0}")]
    InvalidRelayParam(String),
}

/// isekai-helperがハンドシェイクを一切書き出せずに終了した場合に、シェル側から
/// 代わりに出力させるマーカー行。この行が来たら、続く内容はハンドシェイクJSONではなく
/// stderrログ(`launch_and_capture_handshake`が`mktemp -d`で作る一時ディレクトリ内の
/// ログファイル)そのものとして扱う。
const NO_HANDSHAKE_MARKER: &str = "__ISEKAI_HELPER_NO_HANDSHAKE__";

/// `NO_HANDSHAKE_MARKER`とともに返されたisekai-helperのstderrログを見て、
/// `--bind`固定ポート指定時のUDP bind失敗かどうかを分類する。マッチする既知の
/// パターンが無ければ(または`bind_port`が指定されていなければ)従来通り
/// `HandshakeTimeout`のままにする(固定ポート指定と無関係なクラッシュまで
/// bind失敗として誤分類しないため)。
///
/// `isekai-helper`は常にmusl静的リンクバイナリとして配布される(`build-isekai-helper-musl.sh`)。
/// musl libcの`strerror()`はglibcと文言が異なる場合があり、実際`EADDRINUSE`はglibcでは
/// "Address already in use"だがmuslでは"Address in use"("already"が無い)になることを
/// 実機E2Eテストで確認した(開発機のglibc環境で書いた元のパターンは、実際に配布される
/// muslバイナリの出力に一度もマッチしていなかった)。他の2パターン("Permission denied"/
/// "Address not available")は偶然glibc/musl間で表記が一致していたため気づかれていなかった。
fn classify_launch_failure(log_text: &str, bind_port: Option<u16>) -> BootstrapError {
    let Some(port) = bind_port else {
        return BootstrapError::HandshakeTimeout;
    };
    if log_text.contains("Address already in use") || log_text.contains("Address in use") {
        BootstrapError::BindPortInUse(port)
    } else if log_text.contains("Permission denied") {
        BootstrapError::BindPermissionDenied(port)
    } else if log_text.contains("Cannot assign requested address")
        || log_text.contains("Address not available")
    {
        BootstrapError::BindAddressUnavailable(port)
    } else {
        BootstrapError::HandshakeTimeout
    }
}

/// Phase S-0f: 独自定義をやめ、`isekai-protocol`（pure crate、`isekai-ssh`/
/// `isekai-transport` とも共有）の `HandshakeJson` を型として再利用する
/// （フィールド構成は元々同一だった。ISEKAI_SSH_DESIGN.md「共有ロジックの
/// crate 分割」参照）。`stun_observed_addr`/`relay_public_addr` の意味は
/// `isekai_protocol::handshake::HandshakeJson` のdocコメント、または
/// Phase 10 の `isekai_stun_p2p_transport.rs`/`isekai_link_relay_transport.rs`
/// を参照。
pub type HelperHandshake = isekai_protocol::handshake::HandshakeJson;

/// SSH起動コマンドラインに埋め込む、P2P方式ごとの追加引数。3方式は互いに排他
/// （isekai-helper側の`--relay`と`--stun-server`/`--punch-peer`は併用不可、
/// main.rsのparse_argsも参照）であり、enumにすることで呼び出し側が矛盾した
/// 組み合わせ（例: stun_serverとrelay_addrを両方Someにする)を型として表現できない
/// ようにしてある。
#[derive(Debug, Clone, Default)]
pub enum HelperP2pMode {
    #[default]
    None,
    /// Phase 10: STUN+SSHランデブー方式(`TransportPreference::IsekaiStunP2pQuic`)。
    Stun { stun_server: SocketAddr, punch_peer: Option<SocketAddr> },
    /// Phase 10: relay方式(`TransportPreference::IsekaiLinkRelayQuic`)。
    Relay { relay_addr: SocketAddr, relay_sni: String, relay_jwt: String },
}

/// 既知の Linux アーキテクチャ用にビルドした isekai-helper の静的バイナリ。
/// 呼び出し側（Kotlin/Android ビルド成果物、または include_bytes!）が供給する。
pub struct HelperBinaries<'a> {
    pub x86_64: &'a [u8],
    pub aarch64: &'a [u8],
}

impl<'a> HelperBinaries<'a> {
    fn select_for(&self, uname_m: &str) -> Result<&'a [u8], BootstrapError> {
        match uname_m.trim() {
            "x86_64" => Ok(self.x86_64),
            "aarch64" | "arm64" => Ok(self.aarch64),
            other => Err(BootstrapError::UnsupportedArch(other.to_string())),
        }
    }
}

/// 1本の exec チャネルでコマンドを実行し、（任意で）stdin にバイト列を書き込み、
/// stdout を収集して (stdout, exit_status) を返す。
async fn run_exec(
    session: &mut client::Handle<RusshEventHandler>,
    command: &str,
    stdin: Option<&[u8]>,
) -> Result<(Vec<u8>, Option<u32>), BootstrapError> {
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|e| BootstrapError::Exec(format!("channel_open_session: {e}")))?;

    channel
        .exec(true, command)
        .await
        .map_err(|e| BootstrapError::Exec(format!("exec({command:?}): {e}")))?;

    if let Some(data) = stdin {
        channel
            .data(data)
            .await
            .map_err(|e| BootstrapError::Exec(format!("stdin write: {e}")))?;
    }
    channel
        .eof()
        .await
        .map_err(|e| BootstrapError::Exec(format!("eof: {e}")))?;

    let mut stdout = Vec::new();
    let mut exit_status = None;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { data }) => stdout.extend_from_slice(&data),
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                // stderr はログ用途のみ。デバッグレベルで残す。
                if let Ok(s) = std::str::from_utf8(&data) {
                    log::debug!("ssh exec stderr: {s}");
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status: status }) => {
                exit_status = Some(status);
            }
            None => break,
            _ => {}
        }
    }
    Ok((stdout, exit_status))
}

/// `bytes`のSHA-256をhex(lowercase)で返す。
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// isekai-helper が既にインストール済みかつ起動可能なバージョン・チェックサムかを確認する
/// (セキュリティレビュー #67)。
///
/// バージョン文字列の一致だけでは、`~/.local/bin/isekai-helper`
/// への書き込み権限を持つ別ローカルユーザー(または侵害されたアカウント)が、正しい
/// バージョン文字列を出力するトロイ化バイナリに差し替えても検出できない。埋め込み
/// バイナリ(`helper_quic_transport.rs`の`include_bytes!`)から計算した
/// `expected_sha256_hex`とリモート上のバイナリの実際のSHA-256を比較することで、
/// バージョン一致だけでなく「配布したバイナリそのもの」であることを検証する。
///
/// リモートに`sha256sum`が無い環境向けのフォールバック: バージョンのみの一致で
/// 再利用を許可する(多層防御であり必須の防御層ではないため — 攻撃者が既に
/// `~/.local/bin`へ書き込める状態は、通常そのアカウント自体が侵害されている状態に
/// 近く、severity Lowと判断している。詳細はタスク#67のIssue参照)。
async fn check_existing_version(
    session: &mut client::Handle<RusshEventHandler>,
    expected_version: &str,
    expected_sha256_hex: &str,
) -> bool {
    let cmd = format!(
        "test -x {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME} && {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME} --version && \
         (command -v sha256sum >/dev/null 2>&1 && sha256sum {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME} || echo NO_SHA256SUM)"
    );
    match run_exec(session, &cmd, None).await {
        Ok((stdout, Some(0))) => {
            let out = String::from_utf8_lossy(&stdout);
            if !out.contains(expected_version) {
                return false;
            }
            let Some(last_line) = out.lines().last() else {
                return false;
            };
            if last_line.trim() == "NO_SHA256SUM" {
                warn!(
                    "isekai-helper: remote host has no sha256sum, reusing existing install \
                     based on version match only (checksum verification skipped)"
                );
                return true;
            }
            // `sha256sum`の出力形式: "<hex>  <path>"。
            let remote_hash = last_line.split_whitespace().next().unwrap_or("");
            remote_hash.eq_ignore_ascii_case(expected_sha256_hex)
        }
        _ => false,
    }
}

/// バイナリを `~/.local/bin/isekai-helper` へ転送する（base64 + exec、SFTP subsystem 不要）。
/// ファイル権限は 0700（HELPER_PROTOCOL.md のブートストラップ側契約）。
async fn upload_binary(
    session: &mut client::Handle<RusshEventHandler>,
    binary: &[u8],
) -> Result<(), BootstrapError> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(binary);
    let cmd = format!(
        "umask 077 && mkdir -p {HELPER_INSTALL_DIR} && \
         base64 -d > {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}.tmp && \
         chmod 0700 {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}.tmp && \
         mv {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}.tmp {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}"
    );
    let (stdout, exit_status) = run_exec(session, &cmd, Some(encoded.as_bytes())).await?;
    if exit_status != Some(0) {
        return Err(BootstrapError::Upload(format!(
            "upload command exited with {:?}: {}",
            exit_status,
            String::from_utf8_lossy(&stdout)
        )));
    }
    Ok(())
}

/// isekai-helper を起動し、標準出力のハンドシェイク行をポーリングで取得する。
/// `--max-idle-lifetime`/`--idle-timeout` は既定値に任せる（呼び出し側で変えたい場合は
/// target_addr 以外の追加引数を別途サポートする拡張余地を残す）。
async fn launch_and_capture_handshake(
    session: &mut client::Handle<RusshEventHandler>,
    ssh_relay_target: &str,
    bind_port: Option<u16>,
    p2p_mode: &HelperP2pMode,
) -> Result<HelperHandshake, BootstrapError> {
    // ファイル権限は 0700(dir)/0600(handshake ファイル) を umask で保証する
    // （HELPER_PROTOCOL.md「Bootstrap file permissions」契約）。
    //
    // ハンドシェイク/ログの出力先は、呼び出しごとに`mktemp -d`で新規作成する一時
    // ディレクトリにする（固定パス`~/.cache/isekai-terminal/helper.{handshake,log}`を
    // 全セッション共通で使い回していた旧実装には、同一サーバーへ複数タブ/セッションで
    // 接続した際に、まだ生きている前のisekai-helperデーモンが同じファイルのfdを
    // 開いたままの状態で次の起動がそのファイルを`>`で truncateしてしまい、両者の
    // 書き込みが同一ファイル上で衝突してログが破損する不具合があった（実際に
    // `second_session_with_same_fixed_port_fails_as_port_in_use`テストで発見。
    // 壊れた"Error: Address in use (os error 98)"という行——本来なら
    // "Address already in use"のはず——が生成され、classify_launch_failureが
    // BindPortInUseを正しく分類できずHandshakeTimeoutに落ちていた）。
    // `mktemp -d`は呼び出しごとに衝突しない新規ディレクトリを作るため、この種の
    // 共有状態の衝突が構造的に起きなくなる。`trap ... EXIT`で、スクリプトが
    // 正常終了/シグナル受信のどちらで終わっても一時ディレクトリを確実に削除する
    // （SIGKILLはどのプロセスもtrapできないため唯一の例外だが、これはOS一般の制約であり
    // 対処不能）。isekai-helper本体はsetsidで独立したセッションになっており、
    // ディレクトリ削除後もまだ開いているfdへの書き込みは継続できる(Linuxのunlink-while-open
    // 挙動)ため、削除タイミングをデーモンの寿命に合わせる必要はない。
    //
    // 実機検証の結果、`cmd & disown`（引数無しはもちろん `disown -a` でも）では、
    // 起動元シェル（russh の exec チャネルが実行する `bash -c "..."` 本体）が
    // スクリプル終了後も長時間 `do_wait()` に留まり、isekai-helper（長時間稼働する
    // デーモン）の終了を待ち続けてしまうことを確認した。`( ... & )` のようにサブシェルで
    // 一段包んでバックグラウンド化すると、外側シェルの直接の子はサブシェル（即座に終了）
    // だけになり、isekai-helper は孫プロセスとして完全に独立するため、この問題が起きない。
    //
    // `bind_port`: Tailscale経由（path0のみ）は`ts-input`チェーンが素通しなので既定の
    // エフェメラルポート（`0.0.0.0:0`）のままでよいが、direct_host（外部到達アドレス、
    // Wi-Fi/セルラー物理pathも同じ宛先ポートを使う）はホストのiptables許可リストに
    // 載った固定ポートでないと外形疎通できない（実機検証で確認、Phase 9-4）。
    // `[::]:port`（IPv6ワイルドカード）でbindすると、noqの`Endpoint::server()`が
    // 内部で`set_only_v6(false)`する（IPv6アドレスでbindした場合のみの挙動）ため、
    // 同一ソケットがIPv4/IPv6両方のパケットを受け付けるdual-stackになる（実機で
    // 確認済み、Phase 9-4追加調査）。`0.0.0.0`（IPv4のみ）ではこの恩恵が無い。
    let bind_arg = match bind_port {
        Some(port) => format!("--bind [::]:{port} "),
        None => String::new(),
    };
    // セキュリティレビュー #57: リモートログインシェル経由で実行するコマンド文字列に
    // 埋め込む前に、型安全でない`&str`値は全てシェルクォートする(型がSocketAddr/u16の
    // ものはメタ文字を含み得ないため対象外)。`ssh_relay_target`は現状すべての呼び出し元で
    // 固定値("127.0.0.1:22")だが、将来ユーザー入力由来になっても安全なように一律クォートする。
    let quoted_target = shell_single_quote(ssh_relay_target);
    // Phase 10: P2P方式ごとの追加引数。isekai-helper の daemon は起動直後に
    // setsid で stdin を `/dev/null` にリダイレクトするため、対向アドレスやトークンを
    // インタラクティブな stdin プロトコルで後から渡す手段が無い。このため、必要な値は
    // 呼び出し側(isekai-terminal)が事前に用意しておき、起動コマンドラインの引数として
    // そのまま埋め込む（HELPER_PROTOCOL.md 参照）。
    //
    // `relay_jwt`だけは例外: argv経由(`ps aux`/`/proc/<pid>/cmdline`から他のローカル
    // ユーザーに読める)を避け、`session_secret`と同様の扱いにする(セキュリティレビュー
    // #58)。このexecチャネルのstdinへ書き込み、リモート側で`mktemp -d`した一時ディレクトリ
    // 内のファイルに`cat`で保存してから、そのファイルパスを`--relay-jwt-file`として渡す。
    let (p2p_arg, jwt_stdin): (String, Option<Vec<u8>>) = match p2p_mode {
        HelperP2pMode::None => (String::new(), None),
        HelperP2pMode::Stun { stun_server, punch_peer } => {
            let punch = match punch_peer {
                Some(addr) => format!("--punch-peer {addr} "),
                None => String::new(),
            };
            (format!("--stun-server {stun_server} {punch}"), None)
        }
        HelperP2pMode::Relay { relay_addr, relay_sni, relay_jwt } => {
            // セキュリティレビュー #57: シェルクォートに加え、厳格な許容文字集合での
            // 検証も行う(多層防御。relay/JWT発行元が侵害・誤設定された場合でも
            // シェルメタ文字を埋め込めないようにする)。
            validate_relay_sni(relay_sni)
                .map_err(|e| BootstrapError::InvalidRelayParam(e.to_string()))?;
            validate_relay_jwt(relay_jwt)
                .map_err(|e| BootstrapError::InvalidRelayParam(e.to_string()))?;
            let quoted_sni = shell_single_quote(relay_sni);
            (
                format!(
                    "--relay {relay_addr} --relay-sni {quoted_sni} --relay-jwt-file $tmpdir/relay_jwt "
                ),
                Some(relay_jwt.clone().into_bytes()),
            )
        }
    };
    // relay_jwtがある場合のみ、起動前にexecチャネルのstdinから読み取って
    // `$tmpdir/relay_jwt`へ保存する(0600、`umask 077`により保証)。
    let write_jwt_step = if jwt_stdin.is_some() { "cat > $tmpdir/relay_jwt && " } else { "" };
    let sleep_secs = HANDSHAKE_POLL_INTERVAL_MS as f64 / 1000.0;
    // ハンドシェイクが空のまま(=isekai-helperが起動直後にクラッシュした等)なら、
    // ハンドシェイクJSONの代わりにマーカー行+stderrログを返す。呼び出し側
    // (classify_launch_failure)がこれを見て、bind失敗の具体的な理由を推定できる
    // ようにする(単なる`cat`の空出力だけでは原因が一切わからないため)。
    let launch_cmd = format!(
        "umask 077 && tmpdir=$(mktemp -d) && trap 'rm -rf $tmpdir' EXIT && \
         {write_jwt_step}\
         ( setsid {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME} {bind_arg}{p2p_arg}--target {quoted_target} \
         </dev/null >$tmpdir/handshake 2>$tmpdir/log & ); \
         for i in $(seq 1 {HANDSHAKE_POLL_ATTEMPTS}); do \
           [ -s $tmpdir/handshake ] && break; \
           sleep {sleep_secs}; \
         done; \
         if [ -s $tmpdir/handshake ]; then cat $tmpdir/handshake; \
         else echo {NO_HANDSHAKE_MARKER}; cat $tmpdir/log 2>/dev/null; fi"
    );

    let (stdout, _exit_status) = run_exec(session, &launch_cmd, jwt_stdin.as_deref()).await?;
    let mut lines = stdout.split(|&b| b == b'\n').filter(|line| !line.is_empty());
    let first_line = lines.next().ok_or(BootstrapError::HandshakeTimeout)?;

    if first_line == NO_HANDSHAKE_MARKER.as_bytes() {
        let log_text = lines
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join("\n");
        return Err(classify_launch_failure(&log_text, bind_port));
    }

    // `decode_handshake_json` は素の `serde_json::from_slice` と違い、サイズ上限・
    // `v`/`cert_sha256`/`session_secret`/`listen_port` のフォーマット検証も行う
    // （isekai-protocol crate、`isekai_protocol::handshake` のdocコメント参照）。
    // isekai-helper からの応答は SSH exec 経由の外部入力なので、この検証強化は
    // 素直に恩恵がある。
    decode_handshake_json(first_line).map_err(|e| BootstrapError::HandshakeParse(e.to_string()))
}

/// isekai-helper が起動していることを保証し、ハンドシェイクを返す。
/// 既存インストールが使えればそれを再利用し、無ければ配布・起動する。
pub async fn ensure_helper_running(
    session: &mut client::Handle<RusshEventHandler>,
    binaries: &HelperBinaries<'_>,
    expected_version: &str,
    ssh_relay_target: &str,
    bind_port: Option<u16>,
    p2p_mode: &HelperP2pMode,
) -> Result<HelperHandshake, BootstrapError> {
    // チェックサム検証(セキュリティレビュー #67)には、既存インストール確認より前に
    // アーキテクチャを知る必要がある(比較対象のバイナリを選ぶため)。以前は
    // 「既存インストールがバージョン不一致だった場合のみ」uname -mを実行していたが、
    // 常に1回実行するよう変更した(既存sshdへの追加往復は無視できるコスト)。
    let (uname_out, _) = run_exec(session, "uname -m", None).await?;
    let uname_m = String::from_utf8_lossy(&uname_out);
    let binary = binaries.select_for(&uname_m)?;
    let expected_sha256 = sha256_hex(binary);

    if !check_existing_version(session, expected_version, &expected_sha256).await {
        info!("isekai-helper: no matching existing install (version/checksum mismatch), uploading binary for {}", uname_m.trim());
        upload_binary(session, binary).await?;
    } else {
        info!("isekai-helper: existing install matches expected version+checksum, reusing");
    }

    match tokio::time::timeout(
        Duration::from_secs(10),
        launch_and_capture_handshake(session, ssh_relay_target, bind_port, p2p_mode),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            warn!("isekai-helper: launch/handshake timed out");
            Err(BootstrapError::HandshakeTimeout)
        }
    }
}

#[cfg(test)]
mod tests {
    //! ローカルの実 sshd（127.0.0.1:22）に対する E2E テスト。
    //! 実 sshd（127.0.0.1:22）+ 事前に authorized_keys へ登録したテスト用鍵が必要な
    //! 実機 E2E テスト。`HELPER_BOOTSTRAP_TEST_KEY`（鍵ファイルパス）が設定されていない
    //! 環境では自動的にスキップする（CI/他の開発者の `cargo test` を壊さないようにするため。
    //! opt-in 方式であり、明示的な SKIP フラグの有無に依存しない）。
    use super::*;
    use std::sync::Arc;

    fn test_key_path() -> Option<String> {
        std::env::var("HELPER_BOOTSTRAP_TEST_KEY").ok()
    }

    async fn connect_test_session(key_path: &str) -> client::Handle<RusshEventHandler> {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
        // host key は常に承認する（テスト専用）。
        tokio::spawn(async move {
            while let Some(ev) = event_rx.recv().await {
                if let crate::transport::TransportEvent::HostKey(_, reply) = ev {
                    let _ = reply.send(true);
                }
            }
        });

        let config = Arc::new(client::Config::default());
        let handler = RusshEventHandler::new(event_tx);
        let mut session = client::connect(config, ("127.0.0.1", 22), handler)
            .await
            .expect("failed to connect to local sshd on 127.0.0.1:22");

        let key_pem = std::fs::read_to_string(key_path).unwrap();
        let key = russh_keys::PrivateKey::from_openssh(&key_pem).unwrap();
        let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
        let ok = session
            .authenticate_publickey(user, Arc::new(key))
            .await
            .expect("auth request failed");
        assert!(ok, "publickey auth to local sshd failed");
        session
    }

    fn read_musl_binary(target: &str) -> Vec<u8> {
        let path = format!(
            "{}/target/{}/release/isekai-helper",
            env!("CARGO_MANIFEST_DIR"),
            target
        );
        std::fs::read(&path)
            .unwrap_or_else(|e| panic!("failed to read {path} (build it first via rust-core/scripts/build-isekai-helper-musl.sh): {e}"))
    }

    #[tokio::test]
    async fn bootstraps_and_launches_helper_over_real_ssh() {
        let Some(key_path) = test_key_path() else {
            eprintln!(
                "skipping: HELPER_BOOTSTRAP_TEST_KEY not set (requires a real sshd + registered test key)"
            );
            return;
        };
        let mut session = connect_test_session(&key_path).await;

        // クリーンな状態から検証するため、既存インストールを削除しておく。
        let _ = run_exec(
            &mut session,
            &format!("rm -f {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}"),
            None,
        )
        .await;

        let x86_64_bin = read_musl_binary("x86_64-unknown-linux-musl");
        let aarch64_bin = read_musl_binary("aarch64-unknown-linux-musl");
        let binaries = HelperBinaries {
            x86_64: &x86_64_bin,
            aarch64: &aarch64_bin,
        };

        let handshake =
            ensure_helper_running(&mut session, &binaries, "0.1.0", "127.0.0.1:22", None, &HelperP2pMode::None)
                .await
                .expect("bootstrap failed");
        assert_eq!(handshake.v, 1);
        assert!(handshake.listen_port > 0);
        assert_eq!(handshake.cert_sha256.len(), 64);

        // 2回目呼び出し: バイナリは既にインストール済みのはずなので、
        // アップロードをスキップしても正常にハンドシェイクを取得できることを確認する。
        let handshake2 =
            ensure_helper_running(&mut session, &binaries, "0.1.0", "127.0.0.1:22", None, &HelperP2pMode::None)
                .await
                .expect("second bootstrap call failed");
        assert_eq!(handshake2.v, 1);
        // 起動のたびに ephemeral cert/secret を生成するため、値自体は毎回変わる。
        assert_ne!(handshake.session_secret, handshake2.session_secret);
    }

    /// 固定ポート(`--bind`)を指定してブートストラップし、実際にそのポートで
    /// 待ち受けていることを確認する(実 sshd + テスト鍵が必要な opt-in E2E)。
    #[tokio::test]
    async fn bootstraps_with_fixed_bind_port() {
        let Some(key_path) = test_key_path() else {
            eprintln!("skipping: HELPER_BOOTSTRAP_TEST_KEY not set");
            return;
        };
        let mut session = connect_test_session(&key_path).await;
        let _ = run_exec(&mut session, &format!("rm -f {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}"), None).await;

        let x86_64_bin = read_musl_binary("x86_64-unknown-linux-musl");
        let aarch64_bin = read_musl_binary("aarch64-unknown-linux-musl");
        let binaries = HelperBinaries { x86_64: &x86_64_bin, aarch64: &aarch64_bin };

        // OSに割り当てられたエフェメラルレンジと衝突しにくい高番ポートを使う。
        let fixed_port: u16 = 58123;
        let handshake = ensure_helper_running(
            &mut session, &binaries, "0.1.0", "127.0.0.1:22", Some(fixed_port), &HelperP2pMode::None,
        )
        .await
        .expect("bootstrap with fixed bind port failed");
        assert_eq!(handshake.listen_port, fixed_port);
    }

    /// 同一サーバー・同一固定ポートで2セッション目を開いた場合、黙ってエフェメラル
    /// ポートへフォールバックしたりハングしたりせず、`BindPortInUse`として安全に
    /// 失敗することを確認する(実 sshd + テスト鍵が必要な opt-in E2E)。
    #[tokio::test]
    async fn second_session_with_same_fixed_port_fails_as_port_in_use() {
        let Some(key_path) = test_key_path() else {
            eprintln!("skipping: HELPER_BOOTSTRAP_TEST_KEY not set");
            return;
        };
        let mut session1 = connect_test_session(&key_path).await;
        let mut session2 = connect_test_session(&key_path).await;
        let _ = run_exec(&mut session1, &format!("rm -f {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}"), None).await;

        let x86_64_bin = read_musl_binary("x86_64-unknown-linux-musl");
        let aarch64_bin = read_musl_binary("aarch64-unknown-linux-musl");
        let binaries = HelperBinaries { x86_64: &x86_64_bin, aarch64: &aarch64_bin };

        let fixed_port: u16 = 58124;
        let _handshake1 = ensure_helper_running(
            &mut session1, &binaries, "0.1.0", "127.0.0.1:22", Some(fixed_port), &HelperP2pMode::None,
        )
        .await
        .expect("first session should bind the fixed port successfully");

        // 1セッション目がまだ同じ固定ポートで待ち受けている間に、2セッション目を
        // 同じポートで開こうとする。ensure_helper_running は「既存インストール確認」
        // のみ行い実行中プロセスの有無は見ないため、新しいisekai-helperプロセスが
        // 起動を試み、bindの時点で衝突するはず。
        let second_result = ensure_helper_running(
            &mut session2, &binaries, "0.1.0", "127.0.0.1:22", Some(fixed_port), &HelperP2pMode::None,
        )
        .await;

        match second_result {
            Err(BootstrapError::BindPortInUse(port)) => assert_eq!(port, fixed_port),
            other => panic!("expected BindPortInUse({fixed_port}), got {other:?}"),
        }
    }

    // ── classify_launch_failure(純粋関数、実SSH不要) ──────────────────

    #[test]
    fn classify_launch_failure_detects_address_in_use() {
        let log = "Error: Address already in use (os error 98)\n";
        assert!(matches!(
            classify_launch_failure(log, Some(45900)),
            BootstrapError::BindPortInUse(45900)
        ));
    }

    /// `isekai-helper`の実配布物(musl静的リンク)は同じEADDRINUSEでもglibcと違う文言
    /// ("already"が無い)を出す。実機E2Eテストで発見(このパターンが無いと本番では
    /// 常にHandshakeTimeoutへ誤分類されていた)。
    #[test]
    fn classify_launch_failure_detects_address_in_use_musl_wording() {
        let log = "Error: Address in use (os error 98)\n";
        assert!(matches!(
            classify_launch_failure(log, Some(45900)),
            BootstrapError::BindPortInUse(45900)
        ));
    }

    #[test]
    fn classify_launch_failure_detects_permission_denied() {
        let log = "Error: Permission denied (os error 13)\n";
        assert!(matches!(
            classify_launch_failure(log, Some(80)),
            BootstrapError::BindPermissionDenied(80)
        ));
    }

    #[test]
    fn classify_launch_failure_detects_address_unavailable() {
        let log = "Error: Cannot assign requested address (os error 99)\n";
        assert!(matches!(
            classify_launch_failure(log, Some(45900)),
            BootstrapError::BindAddressUnavailable(45900)
        ));
    }

    #[test]
    fn classify_launch_failure_falls_back_to_timeout_for_unknown_reason() {
        let log = "some unrelated crash message\n";
        assert!(matches!(
            classify_launch_failure(log, Some(45900)),
            BootstrapError::HandshakeTimeout
        ));
    }

    #[test]
    fn classify_launch_failure_falls_back_to_timeout_when_no_bind_port_was_requested() {
        // bind_portを指定していない(エフェメラルポート)場合、bind関連の文字列が
        // たまたまログに含まれていてもbind失敗として誤分類しない。
        let log = "Error: Address already in use (os error 98)\n";
        assert!(matches!(classify_launch_failure(log, None), BootstrapError::HandshakeTimeout));
    }
}
