mod attach_arbiter;
mod attach_runtime;
mod resume;

use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use attach_runtime::{AttachRuntime, HelloOutcome};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, cancel_attach_proof_transcript, decode_attach_activate, decode_attach_hello,
    decode_cancel_attach, encode_attach_response, AttachKey, AttachProof, AttachRejectReason, AttachResponse,
    ATTACH_ACTIVATE_FRAME_LEN, ATTACH_HELLO_FRAME_LEN, CANCEL_ATTACH_FRAME_LEN, FRAME_ATTACH_CANCEL,
    FRAME_ATTACH_HELLO,
};
use quicmux::{AnyByteStreamReadHalf, AnyByteStreamWriteHalf, AnyMuxConnection, AnyMuxListener, MuxServerConfig};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use resume::{Session, SessionTable};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

/// `AnyByteStreamReadHalf::read`'s guarantee ("at most `buf.len()`,
/// possibly fewer, `0` on EOF") is weaker than this file's fixed-size frame
/// decoding needs — mirrors `isekai-transport::relay`'s private `read_exact`
/// helper (same project convention: `tests/*_e2e.rs`/crate-internal I/O
/// helpers are deliberately duplicated per crate rather than shared, see
/// that crate's module docs).
async fn read_exact(recv: &mut AnyByteStreamReadHalf, buf: &mut [u8]) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = recv.read(&mut buf[filled..]).await.context("stream read failed")?;
        if n == 0 {
            return Err(anyhow!("stream ended before {} bytes were read (got {filled})", buf.len()));
        }
        filled += n;
    }
    Ok(())
}

// isekai-helper: 認証付き QUIC↔TCP リレー。
// 契約の詳細は /HELPER_PROTOCOL.md、ATTACH v2 の fencing 部分は `#18`
// (`ISEKAI_PIPE_DESIGN.md`) を参照。このファイルはその契約の実装。

type HmacSha256 = Hmac<Sha256>;

/// S→C output buffer の既定上限（HELPER_PROTOCOL.md §7.4 の既定案）。
const DEFAULT_RESUME_BUFFER_SIZE: usize = 4 * 1024 * 1024;

/// `SessionTable` に同時保持できるセッション数の既定上限（Phase S-4b）。
/// 通常運用でこれだけ同時に resume 待ちセッションが積まれることは想定しにくい
/// ため、小さめの値にして DoS/リソース枯渇対策を優先する。
const DEFAULT_MAX_SESSIONS: usize = 16;

const EXPORTER_LABEL: &[u8] = b"isekai-pipe-auth-v1";
const ALPN: &[u8] = b"isekai-pipe/1";

/// 完全に未知のフレーム種別(ATTACH_HELLOでもquicmux::FRAME_RESUMEでもない)
/// を読んだ場合専用。RESUME固有の拒否応答は`quicmux::ResumeRejectReason`
/// (quicmux-server-resume Stage B)に移った。
const FRAME_REJECT_UNSUPPORTED: u8 = 0xFD;

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// See `Args::relay_transport`'s doc comment for the design rationale
/// (evidence-gated opt-in, not a runtime fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RelayTransportKind {
    #[default]
    Udp,
    Qmux,
}

impl std::str::FromStr for RelayTransportKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "udp" => Ok(RelayTransportKind::Udp),
            "qmux" => Ok(RelayTransportKind::Qmux),
            other => Err(anyhow!("invalid --relay-transport value: {other} (expected udp|qmux)")),
        }
    }
}

struct Args {
    target: SocketAddr,
    service_name: String,
    bind: SocketAddr,
    /// `--bind-port-range <START>-<END>`: narrows which UDP port `bind`
    /// (when its own port is `0`, i.e. "OS-assigned") is chosen from, so an
    /// operator can open a small, predictable range in a host firewall
    /// instead of the whole ephemeral port range
    /// (`/proc/sys/net/ipv4/ip_local_port_range` on Linux, which a fresh
    /// `--bind 0.0.0.0:0` draws from by default).
    bind_port_range: Option<(u16, u16)>,
    idle_timeout: u64,
    resume_window: u64,
    max_idle_lifetime: u64,
    /// S→C 方向（helper→client）の resume 用 output buffer 上限。
    resume_buffer_size: usize,
    /// `SessionTable` に同時保持できるセッション数の上限（Phase S-4b）。
    max_sessions: usize,
    once: bool,
    log_level: String,
    /// STUN+SSHランデブー方式のP2P(TransportPreference::IsekaiStunP2pQuic)用。
    /// 設定されていれば起動時にこのSTUNサーバーへ問い合わせ、自分の観測アドレスを
    /// ハンドシェイクJSONの`stun_observed_addr`に含める。
    stun_server: Option<SocketAddr>,
    /// `stun_server`と併用: isekai-terminal側が事前に自分自身のSTUN観測アドレスを
    /// 調べた上で、起動コマンドラインにそのまま埋め込んで渡す(stdin越しの対話的な
    /// 交換は行わない — 対象プロセスはsetsidで即座にデタッチされ、stdinは
    /// /dev/nullにリダイレクトされるため、そもそも対話的なやり取りができない)。
    /// 設定されていれば、ハンドシェイクJSON出力前にこのアドレス宛へ穴あけ用の
    /// probeデータグラムを送出する(simultaneous open)。
    punch_peer: Option<SocketAddr>,
    /// relay経由P2P(TransportPreference::IsekaiLinkRelayQuic)用。設定されていれば
    /// `--bind`する代わりにこのMASQUE relay(`isekai-link-masque`のCONNECT-UDP-bind)
    /// 経由でトンネルを張り、relayが割り当てた公開アドレスをハンドシェイクJSONの
    /// `relay_public_addr`に含める。`--relay-sni`/`--relay-jwt`と併用必須。
    relay: Option<SocketAddr>,
    relay_sni: Option<String>,
    /// `--relay`と併用: relayへの接続に使う下層トランスポート。既定`Udp`は既存の
    /// QUIC-over-UDP(`isekai_link_masque::connect_relay_agent`)。`Qmux`はUDP遮断
    /// 環境向けのQMux-over-TLS-over-TCP経路(`connect_relay_agent_via_qmux`、`#qmux-leg2`)。
    /// `ISEKAI_PIPE_DESIGN.md` Epic G/Hの「single evidence-gated選択、racingなし」方針に
    /// 従い、これは接続開始前の明示的opt-inであり、UDP接続が失敗した場合の自動フォールバック
    /// ではない(そちらは別途 `#@isekai bootstrap-relay transport=qmux` ディレクティブが
    /// isekai-bootstrapの起動コマンドライン組み立て時に静的に選ぶ)。
    relay_transport: RelayTransportKind,
    /// セキュリティレビュー #58: argv経由(`--relay-jwt`)は他のローカルユーザーから
    /// `ps aux`/`/proc/<pid>/cmdline`で読める。後方互換のため引数自体は残すが、
    /// 実際のブートストラップ呼び出し元(`helper_bootstrap.rs`/`isekai-bootstrap::openssh`)
    /// は全て`relay_jwt_file`(ファイル経由)に切り替え済み。
    relay_jwt: Option<String>,
    /// `--relay-jwt`の推奨代替。ファイルパスを受け取り、起動時に一度だけ読み取ってから
    /// 直ちに内容をゼロクリアしunlinkする(`resolve_relay_jwt`参照)。
    relay_jwt_file: Option<String>,
    /// `#20a-3`: `isekai-bootstrap`/`helper_bootstrap.rs`がSSH bootstrap execの
    /// stdin経由で渡す`BootstrapRequestV2`(JSON)のファイルパス。起動時に一度だけ
    /// 読み取り・検証してから unlink する(`resolve_bootstrap_request`参照)。
    /// `client_candidates`は既存の`--punch-peer`と同じ穴あけprobe送出対象に
    /// 追加される(両方指定されていれば両方へ送出する)。
    bootstrap_request_file: Option<String>,
}

fn next_val(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    iter.next()
        .ok_or_else(|| anyhow!("{flag} requires a value"))
}

/// Parses `--bind-port-range <START>-<END>` into an inclusive `(start,
/// end)` pair.
fn parse_bind_port_range(value: &str) -> Result<(u16, u16)> {
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| anyhow!("invalid --bind-port-range value {value:?} (expected <START>-<END>)"))?;
    let start: u16 = start
        .parse()
        .map_err(|_| anyhow!("invalid --bind-port-range start {start:?}"))?;
    let end: u16 = end.parse().map_err(|_| anyhow!("invalid --bind-port-range end {end:?}"))?;
    if start > end {
        return Err(anyhow!("invalid --bind-port-range {value:?}: start must be <= end"));
    }
    Ok((start, end))
}

/// Binds a UDP socket at `bind.ip()`, either at `bind`'s own port (the
/// common case) or, when `port_range` is given, at some free port within
/// that inclusive range — tried in a random order starting from a random
/// offset (so many `isekai-helper` instances started in the same instant
/// don't all race for the same low end of the range) rather than always the
/// first free port found, up to one attempt per port in the range.
fn bind_udp_socket(bind: SocketAddr, port_range: Option<(u16, u16)>) -> std::io::Result<std::net::UdpSocket> {
    let Some((start, end)) = port_range else {
        return std::net::UdpSocket::bind(bind);
    };
    use rand::Rng as _;
    let span = u32::from(end) - u32::from(start) + 1;
    let offset = rand::rngs::OsRng.gen_range(0..span);
    let mut last_err = None;
    for i in 0..span {
        let port = start + ((offset + i) % span) as u16;
        match std::net::UdpSocket::bind(SocketAddr::new(bind.ip(), port)) {
            Ok(socket) => return Ok(socket),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("--bind-port-range {start}-{end} is empty"))
    }))
}

fn print_help() {
    println!("isekai-pipe serve - authenticated QUIC-to-TCP relay (see ISEKAI_PIPE_DESIGN.md)");
    println!();
    println!("USAGE:");
    println!("    isekai-pipe serve [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("    --target <ADDR:PORT>          relay destination (default: 127.0.0.1:22)");
    println!("    --bind <ADDR:PORT>             QUIC bind address (default: 0.0.0.0:0)");
    println!("    --bind-port-range <START>-<END> pick --bind's port from this range instead of");
    println!("                                   an OS-assigned one (requires --bind's port to be 0)");
    println!("    --idle-timeout <SECS>          QUIC transport idle timeout (default: 15)");
    println!("    --resume-window <SECS>         backstop: how long a parked (disconnected) session stays");
    println!("                                   resumable before being reclaimed, once capacity-based LRU");
    println!("                                   eviction (--max-sessions) hasn't already reclaimed it sooner");
    println!("                                   (default: {})", isekai_pipe_core::DEFAULT_RESUME_GRACE_SECS);
    println!("    --resume-buffer-size <BYTES>   S->C replay buffer size per session (default: {DEFAULT_RESUME_BUFFER_SIZE})");
    println!("    --max-idle-lifetime <SECS>     self-exit after this many seconds with no active connection (default: 600)");
    println!("    --max-sessions <N>             max number of concurrently tracked resume sessions (default: {DEFAULT_MAX_SESSIONS});");
    println!("                                   once reached, the oldest parked session is evicted to make room,");
    println!("                                   or the new session is rejected if none are evictable (all active)");
    println!("    --once                         exit after the first connection closes");
    println!(
        "    --stun-server <ADDR:PORT>      query this STUN server for our own observed address"
    );
    println!(
        "                                   (adds \"stun_observed_addr\" to the handshake JSON)"
    );
    println!("    --punch-peer <ADDR:PORT>       peer's own STUN-observed address (requires --stun-server);");
    println!("                                   sends hole-punch probe datagrams to it before listening");
    println!("    --relay <ADDR:PORT>            MASQUE relay to tunnel through instead of --bind directly");
    println!(
        "                                   (adds \"relay_public_addr\" to the handshake JSON);"
    );
    println!("                                   requires --relay-sni and one of --relay-jwt/--relay-jwt-file");
    println!("    --relay-sni <NAME>             TLS SNI / HTTP authority for --relay");
    println!(
        "    --relay-transport <udp|qmux>   transport to the relay itself (default: udp); qmux uses"
    );
    println!(
        "                                   QMux-over-TLS-over-TCP (EXPERIMENTAL, unverified wire"
    );
    println!(
        "                                   compat with the deployed relay) for networks that block"
    );
    println!("                                   outbound UDP; requires --relay");
    println!(
        "    --relay-jwt-file <PATH>        path to a file containing the Bearer token for --relay"
    );
    println!("                                   (preferred: unlike --relay-jwt, never appears in");
    println!("                                   `ps`/`/proc/<pid>/cmdline`; read once at startup and removed)");
    println!("    --relay-jwt <TOKEN>            Bearer token to authenticate to --relay (deprecated: visible");
    println!(
        "                                   to other local users via `ps`/`/proc/<pid>/cmdline`;"
    );
    println!("                                   prefer --relay-jwt-file)");
    println!(
        "    --bootstrap-request-file <PATH> path to a BootstrapRequestV2 JSON file (#20a); its"
    );
    println!(
        "                                   client_candidates are added as additional hole-punch"
    );
    println!("                                   targets alongside --punch-peer");
    println!("    --log-level <LEVEL>            error|warn|info|debug|trace (default: info)");
    println!("    --version                      print version and exit");
    println!("    -h, --help                     print this help message");
}

fn parse_args_from(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut target: SocketAddr = "127.0.0.1:22".parse().unwrap();
    let mut service_name = "ssh".to_string();
    let mut bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let mut bind_port_range: Option<(u16, u16)> = None;
    let mut idle_timeout = 15u64;
    // 元々は「reattach 5回の最悪ケース実測90秒+余裕」から逆算した120秒だった
    // (実機検証Phase 8-4b、各試行のQUIC handshakeタイムアウト15秒×5回+バックオフ
    // 合計15秒)が、これはネットワーク瞬断の検知+再試行予算としては妥当でも、
    // ノートPC/スマホのスリープのような分〜時間オーダーの中断には短すぎた
    // (実機で1時間48分のスリープ後、resumeが即座にUnknownSessionで失敗する
    // 不具合として顕在化)。実際の資源保護は容量ベースのLRU立ち退き
    // (`SessionTable::insert_existing`が`--max-sessions`超過時に最古のparked
    // sessionを立ち退かせる、既存実装)が一次防御として機能しているため、この
    // 値は「本当に誰も戻ってこないセッションを最終的に回収するバックストップ」
    // という位置づけに変更し、trzsz-ssh/tsshd(同種のUDP常駐resumeデーモン)の
    // `UdpAliveTimeout`既定(10日間)に倣った`isekai_pipe_core::
    // DEFAULT_RESUME_GRACE_SECS`をそのまま既定値に使う。
    let mut resume_window = isekai_pipe_core::DEFAULT_RESUME_GRACE_SECS;
    let mut resume_buffer_size = DEFAULT_RESUME_BUFFER_SIZE;
    let mut max_idle_lifetime = 600u64;
    let mut max_sessions = DEFAULT_MAX_SESSIONS;
    let mut once = false;
    let mut log_level = "info".to_string();
    let mut stun_server: Option<SocketAddr> = None;
    let mut punch_peer: Option<SocketAddr> = None;
    let mut relay: Option<SocketAddr> = None;
    let mut relay_sni: Option<String> = None;
    let mut relay_transport = RelayTransportKind::default();
    let mut relay_jwt: Option<String> = None;
    let mut relay_jwt_file: Option<String> = None;
    let mut bootstrap_request_file: Option<String> = None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--target" => {
                target = next_val(&mut iter, "--target")?
                    .parse()
                    .context("invalid --target value")?;
            }
            "--service-name" => service_name = next_val(&mut iter, "--service-name")?,
            "--bind" => {
                bind = next_val(&mut iter, "--bind")?
                    .parse()
                    .context("invalid --bind value")?;
            }
            "--bind-port-range" => {
                bind_port_range = Some(parse_bind_port_range(&next_val(&mut iter, "--bind-port-range")?)?);
            }
            "--stun-server" => {
                stun_server = Some(
                    next_val(&mut iter, "--stun-server")?
                        .parse()
                        .context("invalid --stun-server value")?,
                );
            }
            "--punch-peer" => {
                punch_peer = Some(
                    next_val(&mut iter, "--punch-peer")?
                        .parse()
                        .context("invalid --punch-peer value")?,
                );
            }
            "--relay" => {
                relay = Some(
                    next_val(&mut iter, "--relay")?
                        .parse()
                        .context("invalid --relay value")?,
                );
            }
            "--relay-sni" => relay_sni = Some(next_val(&mut iter, "--relay-sni")?),
            "--relay-transport" => {
                relay_transport = next_val(&mut iter, "--relay-transport")?.parse()?;
            }
            "--relay-jwt" => relay_jwt = Some(next_val(&mut iter, "--relay-jwt")?),
            "--relay-jwt-file" => relay_jwt_file = Some(next_val(&mut iter, "--relay-jwt-file")?),
            "--bootstrap-request-file" => {
                bootstrap_request_file = Some(next_val(&mut iter, "--bootstrap-request-file")?)
            }
            "--idle-timeout" => {
                idle_timeout = next_val(&mut iter, "--idle-timeout")?
                    .parse()
                    .context("invalid --idle-timeout value")?;
            }
            "--resume-window" => {
                resume_window = next_val(&mut iter, "--resume-window")?
                    .parse()
                    .context("invalid --resume-window value")?;
            }
            "--resume-buffer-size" => {
                resume_buffer_size = next_val(&mut iter, "--resume-buffer-size")?
                    .parse()
                    .context("invalid --resume-buffer-size value")?;
                if resume_buffer_size == 0 {
                    return Err(anyhow!("--resume-buffer-size must be at least 1"));
                }
            }
            "--max-idle-lifetime" => {
                max_idle_lifetime = next_val(&mut iter, "--max-idle-lifetime")?
                    .parse()
                    .context("invalid --max-idle-lifetime value")?;
            }
            "--max-sessions" => {
                max_sessions = next_val(&mut iter, "--max-sessions")?
                    .parse()
                    .context("invalid --max-sessions value")?;
                if max_sessions == 0 {
                    return Err(anyhow!("--max-sessions must be at least 1"));
                }
            }
            "--once" => once = true,
            "--log-level" => log_level = next_val(&mut iter, "--log-level")?,
            "--version" => {
                println!("isekai-pipe {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }
    if punch_peer.is_some() && stun_server.is_none() {
        return Err(anyhow!("--punch-peer requires --stun-server"));
    }
    if relay.is_some() && relay_sni.is_none() {
        return Err(anyhow!(
            "--relay requires --relay-sni and one of --relay-jwt/--relay-jwt-file"
        ));
    }
    if relay.is_some() {
        match (&relay_jwt, &relay_jwt_file) {
            (None, None) => {
                return Err(anyhow!(
                    "--relay requires one of --relay-jwt/--relay-jwt-file"
                ))
            }
            (Some(_), Some(_)) => {
                return Err(anyhow!(
                    "--relay-jwt and --relay-jwt-file are mutually exclusive"
                ))
            }
            _ => {}
        }
    }
    if relay.is_some() && (stun_server.is_some() || punch_peer.is_some()) {
        return Err(anyhow!(
            "--relay cannot be combined with --stun-server/--punch-peer (different P2P transports)"
        ));
    }
    if relay.is_none() && relay_transport != RelayTransportKind::Udp {
        return Err(anyhow!("--relay-transport requires --relay"));
    }
    if service_name.is_empty() {
        return Err(anyhow!("--service-name must not be empty"));
    }
    if bind_port_range.is_some() && bind.port() != 0 {
        return Err(anyhow!(
            "--bind-port-range cannot be combined with an explicit non-zero port in --bind"
        ));
    }
    Ok(Args {
        target,
        service_name,
        bind,
        bind_port_range,
        idle_timeout,
        resume_window,
        resume_buffer_size,
        max_idle_lifetime,
        max_sessions,
        once,
        log_level,
        stun_server,
        punch_peer,
        relay,
        relay_sni,
        relay_transport,
        relay_jwt,
        relay_jwt_file,
        bootstrap_request_file,
    })
}

/// `--relay-jwt`/`--relay-jwt-file`のどちらが指定されたかに応じてJWT文字列を解決する。
/// `parse_args`で相互排他・少なくとも一方の存在は検証済み(`--relay`未指定なら両方
/// `None`のままで、この関数は呼ばれない)。
///
/// ファイル経由の場合(推奨、セキュリティレビュー #58): 読み取り後直ちにファイルを
/// unlinkし、読み取ったバッファもベストエフォートでゼロクリアする(呼び出し元の
/// `helper_bootstrap.rs`/`isekai-bootstrap::openssh`が`mktemp -d`で作る一時
/// ディレクトリは`trap ... EXIT`でも最終的に回収されるが、露出時間を最小化する)。
fn resolve_relay_jwt(relay_jwt: Option<String>, relay_jwt_file: Option<String>) -> Result<String> {
    match (relay_jwt, relay_jwt_file) {
        (Some(jwt), None) => Ok(jwt),
        (None, Some(path)) => {
            let mut content = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read --relay-jwt-file {path}"))?;
            // ベストエフォートで即座に削除する(削除に失敗しても致命的ではない —
            // 呼び出し元シェルの`trap ... EXIT`が一時ディレクトリごと最終的に回収する)。
            if let Err(e) = std::fs::remove_file(&path) {
                log::warn!("failed to remove --relay-jwt-file {path} after reading: {e}");
            }
            let trimmed = content.trim_end_matches(['\n', '\r']).to_string();
            zeroize_string(&mut content);
            Ok(trimmed)
        }
        (None, None) | (Some(_), Some(_)) => {
            unreachable!("relay_jwt/relay_jwt_file exclusivity already validated in parse_args")
        }
    }
}

/// `s`のバイト列をその場でゼロ埋めする、ベストエフォートのメモリスクラブ。
/// 全バイトを`0x00`(有効なASCII/UTF-8)で上書きするため`String`の不変条件
/// (UTF-8妥当性)を壊さない。`write_volatile`はコンパイラによる dead-store
/// elimination を抑止する意図(完全な保証ではないが、ここでは多層防御の
/// 一部でしかないため十分)。
/// `#20a-3`: reads and validates a `BootstrapRequestV2` from `path`, then
/// unlinks the file (matching `resolve_relay_jwt`'s "read once, remove
/// immediately" pattern — the caller's `mktemp -d`/`trap ... EXIT` also
/// reclaims it eventually, this just minimizes exposure time). A malformed
/// request fails the whole startup (all-or-nothing, matching
/// `decode_bootstrap_request_v2`'s own contract) rather than silently
/// continuing without candidates — something is clearly wrong with the SSH
/// bootstrap pipeline itself if this file doesn't parse.
fn resolve_bootstrap_request(path: &str) -> Result<isekai_protocol::BootstrapRequestV2> {
    let bytes = std::fs::read(path).with_context(|| format!("failed to read --bootstrap-request-file {path}"))?;
    if let Err(e) = std::fs::remove_file(path) {
        log::warn!("failed to remove --bootstrap-request-file {path} after reading: {e}");
    }
    isekai_protocol::bootstrap_request::decode_bootstrap_request_v2(&bytes)
        .with_context(|| format!("invalid BootstrapRequestV2 in --bootstrap-request-file {path}"))
}

/// `#20a-3`: `client_candidates.endpoint` is already validated (during
/// `decode_bootstrap_request_v2`) to parse as a `SocketAddr` — this just
/// does that parse again to get the typed value for punching. A candidate
/// whose `endpoint` somehow fails to parse here anyway (defensive; should
/// be unreachable given the earlier validation) is skipped with a warning
/// rather than failing the whole startup over one bad entry.
fn client_candidate_punch_targets(request: &isekai_protocol::BootstrapRequestV2) -> Vec<SocketAddr> {
    request
        .client_candidates
        .iter()
        .filter_map(|candidate| match candidate.endpoint.parse::<SocketAddr>() {
            Ok(addr) => Some(addr),
            Err(e) => {
                log::warn!("bootstrap request candidate endpoint {:?} failed to parse, skipping: {e}", candidate.endpoint);
                None
            }
        })
        .collect()
}

fn zeroize_string(s: &mut String) {
    // SAFETY: 全バイトを0x00で上書きするだけであり、長さ・容量は変えないため
    // UTF-8妥当性は保たれる(0x00は単独で有効なUTF-8バイト列)。
    let bytes = unsafe { s.as_mut_vec() };
    for b in bytes.iter_mut() {
        unsafe { std::ptr::write_volatile(b as *mut u8, 0) };
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub async fn run_from_args(args: impl IntoIterator<Item = String>) -> Result<()> {
    let args = parse_args_from(args)?;

    // ログは stderr にのみ出力する。stdout はハンドシェイク JSON 専用。
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&args.log_level))
        .target(env_logger::Target::Stderr)
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("failed to install rustls ring crypto provider"))?;

    // `#20a-3`: `--bootstrap-request-file`は`--relay`/直接P2Pのどちらの起動でも
    // `isekai-bootstrap::openssh`から常に渡され得る(呼び出し元は起動方式を問わず
    // 送信する)ため、branch分岐より前にここで解決する。実際に`client_candidates`を
    // 穴あけprobeへ使うのは非relay分岐のみ(下記参照) — relay分岐では黙って無視する。
    let bootstrap_request = match &args.bootstrap_request_file {
        Some(path) => Some(resolve_bootstrap_request(path)?),
        None => None,
    };

    // session_secret をランダム生成する（CLI 引数や環境変数には載せない）。
    let mut session_secret = [0u8; 32];
    {
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut session_secret);
    }

    // 起動のたびに ephemeral な自己署名証明書を生成する（永続化しない）。
    let CertifiedKey { cert, key_pair } =
        generate_simple_self_signed(vec!["isekai-pipe.local".to_string()])?;
    let cert_der = cert.der().clone();
    let cert_sha256 = {
        let mut hasher = Sha256::new();
        hasher.update(&cert_der);
        hex_lower(&hasher.finalize())
    };
    let key_der = key_pair.serialize_der();

    let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der)];
    let key = rustls::pki_types::PrivateKeyDer::try_from(key_der)
        .map_err(|e| anyhow!("failed to build private key: {e}"))?;

    // data stream（Phase 7）+ control stream（Phase 8、resume 用）の 2 本を許可する
    // （HELPER_PROTOCOL.md §7.1）。3 本目以降は Phase 7 と同様 reset される。
    // Phase 9-1: multipath 対応。既存 quinn クライアント（Phase 7/8）は
    // open_path() を呼ばないため path0 のみで従来通り動作し、後方互換に
    // 影響しない（Phase 9-0 の compat_check.rs で実証済み）。preferred_address は
    // 明示的に設定しない（QUIC-Exfil 対策、既定で未使用、quicmuxのMuxServerConfigにも
    // その概念がない）。0-RTT / early dataはクライアント・サーバー双方で無効化する契約
    // （HELPER_PROTOCOL.md参照、quicmuxのnoq_server_config内で常に無効化される)。
    let server_config = MuxServerConfig {
        alpn: ALPN.to_vec(),
        exporter_label: EXPORTER_LABEL.to_vec(),
        max_idle_timeout: Duration::from_secs(args.idle_timeout),
        keep_alive_interval: Duration::from_secs((args.idle_timeout / 3).max(1)),
        max_concurrent_bidi_streams: 2,
        max_concurrent_uni_streams: 0,
        multipath: true,
        // `isekai-pipe serve`'s SSH tunnel never sends QUIC datagrams today
        // — see `quicmux`'s `MuxClientConfig::datagram_send_buffer_size` docs.
        datagram_send_buffer_size: None,
        cert_chain,
        private_key: key,
    };

    let (listener, stun_observed_addr, relay_public_addr) = if let Some(relay_addr) = args.relay {
        // relay版P2P(TransportPreference::IsekaiLinkRelayQuic): 自前でbindする代わりに
        // MASQUE relayへCONNECT-UDP-bindトンネルを張り、relayが割り当てた公開アドレスを
        // isekai-terminal側へ(SSHブートストラップのハンドシェイクJSON経由で)伝える。
        // isekai-terminal自身はrelay/MASQUEを一切意識せず、この公開アドレスへ普通にQUIC
        // 接続するだけでよい(isekai_link_relay_transport.rs参照)。
        let relay_sni = args.relay_sni.expect("validated in parse_args");
        let relay_jwt = resolve_relay_jwt(args.relay_jwt, args.relay_jwt_file)?;
        let (relay_socket, proxy_public_address) = match args.relay_transport {
            RelayTransportKind::Udp => isekai_link_masque::connect_relay_agent(relay_addr, &relay_sni, &relay_jwt)
                .await
                .map_err(|e| anyhow!("relay connect failed: {e}"))?,
            RelayTransportKind::Qmux => {
                isekai_link_masque::connect_relay_agent_via_qmux(relay_addr, &relay_sni, &relay_jwt)
                    .await
                    .map_err(|e| anyhow!("relay connect (qmux) failed: {e}"))?
            }
        };
        log::info!(
            "relay: tunnel established via {:?}, proxy_public_address={proxy_public_address}",
            args.relay_transport
        );
        let listener = AnyMuxListener::from_abstract_socket_noq(server_config, Box::new(relay_socket))?;
        (listener, None, Some(proxy_public_address))
    } else {
        // 自前でbindしたソケットを、noqへ渡す前にSTUN問い合わせ・穴あけprobeへ使う
        // （bind_faulty_udp_socket的なラップをする前の生ソケットとして扱う唯一の機会 ——
        // 一度 noq::Endpoint に渡すと、以後の recv は全て noq 自身の poll_recv が
        // 消費してしまい、こちらから直接 recv_from で読むと競合する）。
        let std_socket = bind_udp_socket(args.bind, args.bind_port_range)?;
        std_socket.set_nonblocking(true)?;
        let raw_socket = Arc::new(tokio::net::UdpSocket::from_std(std_socket)?);

        let stun_observed_addr = match args.stun_server {
            Some(stun_server) => match isekai_stun::query_stun(&raw_socket, stun_server).await {
                Ok(addr) => {
                    log::info!("stun: observed address is {addr} (via {stun_server})");
                    Some(addr)
                }
                Err(e) => {
                    log::warn!("stun: query to {stun_server} failed: {e}, continuing without it");
                    None
                }
            },
            None => None,
        };

        // `#20a-3`: `--punch-peer`(単一・既存)と`--bootstrap-request-file`由来の
        // `client_candidates`(複数・新規)を同じ穴あけprobe送出対象として合流させる。
        let mut punch_targets: Vec<SocketAddr> = Vec::new();
        if let Some(peer) = args.punch_peer {
            punch_targets.push(peer);
        }
        if let Some(request) = &bootstrap_request {
            punch_targets.extend(client_candidate_punch_targets(request));
        }
        if !punch_targets.is_empty() && args.stun_server.is_none() {
            return Err(anyhow!(
                "--punch-peer/--bootstrap-request-file candidates require --stun-server"
            ));
        }

        if !punch_targets.is_empty() {
            log::info!("punch: sending hole-punch probes to {punch_targets:?}");
            // 中身はNAT越え専用のマーカーで構わない(相手はQUICパケットとして解釈できない
            // 限り黙って破棄するだけであり、応答は期待しない・待たない)。simultaneous
            // openの意図を保つため、対象ごとに入れ子ループでsleepするのではなく、
            // 1ラウンドで全対象へ送出してからまとめてsleepする。
            for _ in 0..5 {
                for target in &punch_targets {
                    let _ = raw_socket.send_to(b"isekai-punch", *target).await;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }

        // `raw_socket`のArc参照はSTUN問い合わせ・穴あけprobeの間ずっと`&raw_socket`で
        // 借用しているだけ(別変数へclone/moveしていない)ため、ここに至った時点で
        // 参照カウントは必ず1 — `wrap_bound_socket_noq`が受け取れる所有された
        // `tokio::net::UdpSocket`を安全に取り戻せる。
        let raw_socket = Arc::try_unwrap(raw_socket)
            .map_err(|_| anyhow!("raw_socket unexpectedly has more than one owner"))?;
        let listener = AnyMuxListener::wrap_bound_socket_noq(server_config, raw_socket).await?;
        (listener, stun_observed_addr, None)
    };
    let listen_port = listener.local_addr()?.port();
    let session_secret_b64 = base64::engine::general_purpose::STANDARD.encode(session_secret);
    let stun_observed_addr_json = stun_observed_addr.map(|a| a.to_string());
    let relay_public_addr_json = relay_public_addr.map(|a| a.to_string());

    let mut candidates = vec![serde_json::json!({
        "kind": "direct-by-bootstrap-host",
        "port": listen_port,
        "source": "bootstrap-ssh",
    })];
    if let Some(addr) = &stun_observed_addr_json {
        candidates.push(serde_json::json!({
            "kind": "server-reflexive",
            "endpoint": addr,
            "source": "stun",
        }));
    }
    if let Some(addr) = &relay_public_addr_json {
        candidates.push(serde_json::json!({
            "kind": "relayed",
            "endpoint": addr,
            "source": "isekai-link-relay",
        }));
    }

    // 起動ハンドシェイク JSON を stdout に1行だけ出力し、明示的に flush する。
    let handshake = serde_json::json!({
        "v": 1,
        "session_secret": session_secret_b64,
        "protocol": {
            "name": "isekai-pipe",
            "alpn": "isekai-pipe/1",
        },
        "peer": {
            "server_identity": {
                "kind": "quic-cert-sha256",
                "cert_sha256": cert_sha256,
            },
        },
        "services": [
            {
                "name": args.service_name,
                "target": args.target.to_string(),
            },
        ],
        "candidates": candidates,
    });
    // `#20a-4`: when this launch carried a `BootstrapRequestV2` (real bootstrap
    // callers always send one, `#20a-2`), wrap the handshake in a
    // `BootstrapReportV2` envelope echoing back its `session_id`/
    // `bootstrap_attempt_id` rather than adding fields to the handshake
    // itself (module docs on `isekai_protocol::bootstrap_request`). Without a
    // request (direct/manual invocation, e.g. this crate's own e2e tests),
    // keep emitting the bare `HandshakeJson` exactly as before.
    let output_line = match &bootstrap_request {
        Some(request) => serde_json::json!({
            "v": isekai_protocol::BOOTSTRAP_PROTOCOL_V2,
            "session_id": request.session_id,
            "bootstrap_attempt_id": request.bootstrap_attempt_id,
            "handshake": handshake,
        })
        .to_string(),
        None => handshake.to_string(),
    };
    {
        // 1回の write_all にまとめて呼ぶ(本文と改行を別々の書き込みにしない)ことで、
        // シェル側の`[ -s $tmpdir/handshake ]`ポーリングが書きかけの断片を
        // 観測しないことを保証する(このJSON1行は行を跨がないため、単一の
        // write()システムコールで完結すれば十分)。
        let mut line = output_line.into_bytes();
        line.push(b'\n');
        let mut stdout = std::io::stdout();
        stdout.write_all(&line).context("failed to write handshake to stdout")?;
        stdout.flush().context("failed to flush stdout handshake")?;
    }

    log::info!(
        "isekai-helper listening on udp/{} (target={}, cert_sha256={})",
        listener.local_addr()?,
        args.target,
        cert_sha256
    );

    let attach_runtime = AttachRuntime::new(args.target);
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let idle_shutdown = Arc::new(Notify::new());
    // Phase 8: resume 可能セッションのテーブル（session_id → output buffer 等）。
    // Phase S-4b: 同時保持数を `--max-sessions` で上限を設ける（DoS/リソース枯渇対策）。
    let sessions = SessionTable::with_max_sessions(args.max_sessions);

    // Phase 8-3/8-4: park された（data stream が切れて resume 待ちの）セッションの
    // 定期掃除。`--resume-window` の長さだけ resume が来なければ TCP を close して
    // session を破棄する（HELPER_PROTOCOL.md §7.5）。
    //
    // `--idle-timeout`（QUIC transport の生存確認）とは意図的に別の値にしてある。
    // 実機検証（Phase 8-4b）で、この2つを同じ値で共用していると「クライアントが
    // QUIC connection の喪失を検知する（`--idle-timeout` 待ち）頃には、既に
    // park セッションが破棄済み」というタイミング不整合が起き、reattach が
    // 必ず REJECT_UNKNOWN_SESSION になる致命的な不具合を確認した。resume-window は
    // 検知にかかる時間 + reattach のリトライ予算（指数バックオフ計15秒）より
    // 十分長くなければならない、という下限の理屈は変わらないが、この掃除自体は
    // もはや一次防御ではない——`SessionTable::insert_existing`の容量ベースLRU
    // 立ち退き（`--max-sessions`超過時に最古のparked sessionを立ち退かせる）が
    // 資源圧迫時にはこちらより先に効く。この掃除は「容量に余裕があるままいつまでも
    // 戻ってこないセッション」を最終的に回収するためだけのバックストップなので、
    // 既定値は`isekai_pipe_core::DEFAULT_RESUME_GRACE_SECS`（trzsz-ssh/tsshdの
    // `UdpAliveTimeout`に倣った10日間）まで伸ばしてある。
    {
        let sessions = sessions.clone();
        let attach_runtime = attach_runtime.clone();
        let max_parked = Duration::from_secs(args.resume_window);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let expired = sessions.sweep_expired_parked(max_parked).await;
                // `SessionTable::sweep_expired_parked`'s docs: discarding a
                // parked session here doesn't by itself free the matching
                // `AttachArbiter` fencing slot — do that here so a park
                // timing out actually lets a fresh ATTACH for this target
                // through again, instead of leaving it permanently rejected
                // with `BUSY_OTHER_SESSION` until this process restarts.
                for id in expired {
                    if let Some(lease) = attach_runtime.established_lease_for(isekai_protocol::SessionId::from_bytes(id)).await {
                        attach_runtime.relay_ended(lease).await;
                    }
                }
            }
        });
    }

    // --max-idle-lifetime の監視タスク。
    // アクティブな接続が無く、かつ最後の接続終了（または起動）からこの秒数が経過したら自己終了する。
    {
        let attach_runtime = attach_runtime.clone();
        let last_activity = last_activity.clone();
        let max_idle = Duration::from_secs(args.max_idle_lifetime);
        let idle_shutdown = idle_shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if !attach_runtime.is_vacant().await {
                    continue;
                }
                let elapsed = last_activity.lock().await.elapsed();
                if elapsed >= max_idle {
                    log::info!("no active connection for {elapsed:?}, self-terminating");
                    idle_shutdown.notify_one();
                    break;
                }
            }
        });
    }

    let once = args.once;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                log::info!("shutdown requested, closing endpoint");
                listener.close(b"shutdown");
                break;
            }
            _ = idle_shutdown.notified() => {
                log::info!("max-idle-lifetime reached, closing endpoint");
                listener.close(b"idle-timeout");
                break;
            }
            incoming = listener.accept() => {
                let Some(incoming) = incoming else { break };
                let target = args.target;
                let secret = session_secret;
                let attach_runtime = attach_runtime.clone();
                let last_activity = last_activity.clone();
                let sessions = sessions.clone();
                let resume_buffer_size = args.resume_buffer_size;
                let max_resume_grace_secs = args.resume_window;
                let handle_incoming = async move {
                    match incoming.accept().await {
                        Ok(conn) => {
                            let remote = conn.remote_addr();
                            log::info!("QUIC connection established from {remote:?}");
                            if let Err(e) = handle_connection(conn, target, secret, attach_runtime, sessions, resume_buffer_size, max_resume_grace_secs).await {
                                log::warn!("connection from {remote:?} ended: {e:#}");
                            }
                        }
                        Err(e) => log::warn!("failed to accept connection: {e:#}"),
                    }
                    *last_activity.lock().await = Instant::now();
                };
                if once {
                    // `listener.accept()`はハンドシェイク未完了の候補しか返さない
                    // (実際のハンドシェイク完了は`handle_incoming`内の`incoming.accept()`)。
                    // 以前は`tokio::spawn`した直後にここで`listener.close()`していたため、
                    // spawnされたタスクが最初にpollされる前にlistener自体が閉じてしまい、
                    // `--once`が自分自身が処理するはずだった最初の接続を常に道連れに
                    // していた(netlab PoCで実netns越しに実バイナリを繋いで発見)。
                    // 1接続しか処理しない契約なので、ここでは同期的にawaitしてから閉じる。
                    handle_incoming.await;
                    listener.close(b"once");
                    break;
                }
                tokio::spawn(handle_incoming);
            }
        }
    }

    listener.wait_idle().await;
    Ok(())
}

async fn handle_connection(
    conn: AnyMuxConnection,
    target: SocketAddr,
    session_secret: [u8; 32],
    attach_runtime: Arc<AttachRuntime>,
    sessions: SessionTable,
    resume_buffer_size: usize,
    max_resume_grace_secs: u64,
) -> Result<()> {
    // 最初の1バイトでフレーム種別（ATTACH_HELLO=新規 / RESUME=reattach）を
    // 判定してから、種別に応じた残りバイト数を読む。いずれも一定時間内に
    // 届かなければ connection を close する（QUIC connection だけ張って
    // stream を開かない妨害を防ぐ）。
    let (send, recv, frame_type, rest) = tokio::time::timeout(HELLO_TIMEOUT, async {
        let stream = conn.accept_bi().await.context("no stream opened")?;
        let (mut recv, send) = stream.split();
        let mut type_byte = [0u8; 1];
        read_exact(&mut recv, &mut type_byte)
            .await
            .context("failed to read frame type")?;
        let rest_len = match type_byte[0] {
            FRAME_ATTACH_HELLO => ATTACH_HELLO_FRAME_LEN - 1,
            FRAME_ATTACH_CANCEL => CANCEL_ATTACH_FRAME_LEN - 1,
            // quicmux::FRAME_RESUME(0x01)のボディは可変長(token/auth_blobが
            // それぞれ長さ接頭辞つき)なので、ここでは読まない —
            // handle_resume_streamがquicmux::decode_resume_request経由で
            // 自分で読む。
            _ => 0,
        };
        let mut rest = vec![0u8; rest_len];
        if rest_len > 0 {
            read_exact(&mut recv, &mut rest)
                .await
                .context("failed to read frame body")?;
        }
        Ok::<_, anyhow::Error>((send, recv, type_byte[0], rest))
    })
    .await
    .context("HELLO timeout")??;

    match frame_type {
        FRAME_ATTACH_HELLO => {
            let mut hello_bytes = [0u8; ATTACH_HELLO_FRAME_LEN];
            hello_bytes[0] = FRAME_ATTACH_HELLO;
            hello_bytes[1..].copy_from_slice(&rest);
            handle_attach_stream(
                conn,
                send,
                recv,
                hello_bytes,
                target,
                session_secret,
                attach_runtime,
                sessions,
                resume_buffer_size,
                max_resume_grace_secs,
            )
            .await
        }
        quicmux::FRAME_RESUME => {
            handle_resume_stream(conn, send, recv, target, session_secret, attach_runtime, sessions).await
        }
        FRAME_ATTACH_CANCEL => {
            let mut cancel_bytes = [0u8; CANCEL_ATTACH_FRAME_LEN];
            cancel_bytes[0] = FRAME_ATTACH_CANCEL;
            cancel_bytes[1..].copy_from_slice(&rest);
            handle_cancel_attach(conn, cancel_bytes, session_secret, attach_runtime).await
        }
        other => {
            let mut send = send;
            reject(&mut send, &[FRAME_REJECT_UNSUPPORTED]).await;
            Err(anyhow!("unexpected frame type: {other:#x}"))
        }
    }
}

/// `CANCEL_ATTACH`(best-effort、`#18`): 完全一致した`(session_id, generation,
/// attempt_id)`だけを対象にリソースの早期解放を試みる。届かなくても
/// `AttachRuntime`のpending-activationタイマー等が最終的に安全側へ収束する
/// ため、応答フレームは送らないfire-and-forgetでよい。
async fn handle_cancel_attach(
    conn: AnyMuxConnection,
    cancel_bytes: [u8; CANCEL_ATTACH_FRAME_LEN],
    session_secret: [u8; 32],
    attach_runtime: Arc<AttachRuntime>,
) -> Result<()> {
    let cancel = decode_cancel_attach(&cancel_bytes).context("failed to decode CancelAttach")?;
    let transcript = cancel_attach_proof_transcript(&cancel.session_id, cancel.generation, &cancel.attempt_id);
    let expected = compute_attach_proof(&conn, &session_secret, &transcript).await?;
    if !cancel.proof.ct_eq(&AttachProof::new(expected)) {
        return Err(anyhow!("CancelAttach proof mismatch, ignoring"));
    }
    let key = AttachKey { session_id: cancel.session_id, generation: cancel.generation, attempt_id: cancel.attempt_id };
    attach_runtime.cancel(key).await;
    Ok(())
}

/// 拒否フレームを送出し、`shutdown()` 後に `wait_for_close()` で peer への到達を
/// 待ってから返す。これをせずに呼び出し元が即座に `conn` を drop すると、応答が
/// 飛ぶ前に QUIC connection が暗黙に閉じられ、client 側が payload を読めないことが
/// ある（実測で確認済みのバグ、`quicmux::AnyByteStream::wait_for_close`のdocsに同種の
/// 再現記録あり）。`ATTACH_HELLO`のreject語彙(#18)は`STALE_GENERATION`のように
/// 1byteを超える場合があるため、単一byteではなく`&[u8]`を受け取る。
async fn reject(send: &mut AnyByteStreamWriteHalf, bytes: &[u8]) {
    if send.write_all(bytes).await.is_ok() {
        let _ = send.shutdown().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), send.wait_for_close()).await;
    }
}

async fn reject_attach(send: &mut AnyByteStreamWriteHalf, reason: AttachRejectReason) {
    reject(send, &encode_attach_response(&AttachResponse::Reject(reason))).await;
}

/// helper 側の `ATTACH_HELLO` 検証・fencing判定([`AttachRuntime::hello`])・
/// `AttachActivate`待ち・中継を行う(`#18`)。`PendingActivation`(ACK送信後
/// `AttachActivate`受信前)の間は、まだtargetへのSSHユーザーデータを一切
/// 流さない — `#12`で見つかった曖昧区間の修正そのもの。
#[allow(clippy::too_many_arguments)]
async fn handle_attach_stream(
    conn: AnyMuxConnection,
    mut send: AnyByteStreamWriteHalf,
    mut recv: AnyByteStreamReadHalf,
    hello_bytes: [u8; ATTACH_HELLO_FRAME_LEN],
    target: SocketAddr,
    session_secret: [u8; 32],
    attach_runtime: Arc<AttachRuntime>,
    sessions: SessionTable,
    resume_buffer_size: usize,
    max_resume_grace_secs: u64,
) -> Result<()> {
    let hello = match decode_attach_hello(&hello_bytes) {
        Ok(hello) => hello,
        Err(e) => {
            reject_attach(&mut send, AttachRejectReason::Unsupported).await;
            return Err(anyhow!("failed to decode ATTACH_HELLO: {e}"));
        }
    };

    let transcript = attach_hello_proof_transcript(
        &hello.session_id,
        hello.generation,
        &hello.attempt_id,
        hello.requested_resume_grace_secs,
    );
    let expected = compute_attach_proof(&conn, &session_secret, &transcript).await?;
    if !hello.proof.ct_eq(&AttachProof::new(expected)) {
        reject_attach(&mut send, AttachRejectReason::Auth).await;
        return Err(anyhow!("ATTACH_HELLO proof mismatch, rejecting"));
    }

    let key = AttachKey { session_id: hello.session_id, generation: hello.generation, attempt_id: hello.attempt_id };
    let (lease, attach_token) = match attach_runtime.hello(key).await {
        HelloOutcome::Reject(reason) => {
            reject_attach(&mut send, reason).await;
            return Err(anyhow!("ATTACH_HELLO rejected: {reason:?}"));
        }
        HelloOutcome::Ready { lease, attach_token } => (lease, attach_token),
    };

    // クライアントが希望する resume-grace 期間（0 = 希望なし）。この値を
    // そのまま信用してsession保持時間(≒リソース消費)を決めるのではなく、
    // このサーバー自身の `--resume-window`（`max_resume_grace_secs`）で
    // clampした上でACKに実効値を返す（ISEKAI_PIPE_DESIGN.md — client任せに
    // しない設計）。
    let negotiated_resume_grace_secs =
        effective_resume_grace(hello.requested_resume_grace_secs, max_resume_grace_secs);
    let ready = AttachResponse::Ready {
        session_id: hello.session_id,
        generation: hello.generation,
        attempt_id: hello.attempt_id,
        negotiated_resume_grace_secs,
        attach_token,
    };
    send.write_all(&encode_attach_response(&ready)).await.context("failed to write AttachReadyV2")?;

    // `AttachActivate`を待つ。ここで届かなくても(timeout・切断・不一致)、
    // `AttachRuntime`自身のpending-activationタイマーが最終的にサーバー側の
    // stateを安全に後始末する — この読み取りタイムアウトはこの接続タスク
    // 自身が諦めるタイミングを決めるだけで、正しさはそちらに依存しない。
    let activate = tokio::time::timeout(HELLO_TIMEOUT, async {
        let mut buf = [0u8; ATTACH_ACTIVATE_FRAME_LEN];
        read_exact(&mut recv, &mut buf).await.context("failed to read AttachActivate")?;
        decode_attach_activate(&buf).context("failed to decode AttachActivate")
    })
    .await;

    let tcp = match activate {
        Ok(Ok(activate))
            if activate.session_id == key.session_id
                && activate.generation == key.generation
                && activate.attempt_id == key.attempt_id =>
        {
            attach_runtime.activate(key, activate.attach_token).await
        }
        Ok(Ok(_)) => None,
        Ok(Err(e)) => {
            log::info!("failed to decode AttachActivate: {e:#}");
            None
        }
        Err(_) => {
            log::info!("AttachActivate not received within timeout");
            None
        }
    };
    let Some(tcp) = tcp else {
        return Err(anyhow!("attach never activated (timeout, decode failure, or superseded)"));
    };

    let (tcp_read, tcp_write) = tcp.into_split();
    let handle = Arc::new(Mutex::new(Session::new(resume_buffer_size)));
    let session_id_bytes = *hello.session_id.as_bytes();
    if let resume::InsertOutcome::InsertedAfterEvicting(evicted_id) =
        sessions.insert_existing(session_id_bytes, handle.clone()).await
    {
        // See `SessionTable::insert_existing`'s docs: evicting a parked
        // session from the table doesn't by itself free its matching
        // `AttachArbiter` fencing slot — do that here, exactly like the
        // `sweep_expired_parked` fix (`resume.rs`'s docs on that method).
        if let Some(lease) = attach_runtime.established_lease_for(isekai_protocol::SessionId::from_bytes(evicted_id)).await {
            attach_runtime.relay_ended(lease).await;
        }
    }
    log::info!("attach established, session_id={}", hex_lower(&session_id_bytes));

    // control stream(APP_ACK用)は既知のsession_idを再利用するだけで、新規
    // 発行は行わない(#18-4: session_idはクライアントがATTACH_HELLOで既に
    // 決めている)。8-1と同じ理由で、確立を待たずに中継を先に始める。
    let control_task = {
        let conn = conn.clone();
        let handle = handle.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(HELLO_TIMEOUT, accept_control_stream(&conn, session_secret, session_id_bytes))
                .await
            {
                Ok(Ok((csend, crecv))) => {
                    log::info!("control stream established, session_id={}", hex_lower(&session_id_bytes));
                    spawn_app_ack_tasks(csend, crecv, handle);
                }
                Ok(Err(e)) => log::info!("no resume support for this connection ({e:#})"),
                Err(_) => log::info!("control stream not opened within timeout, continuing without resume support"),
            }
        })
    };

    let outcome = relay_buffered(&mut send, &mut recv, tcp_read, tcp_write, handle.clone(), target).await;
    control_task.abort();

    match outcome {
        RelayOutcome::TcpDied => {
            attach_runtime.relay_ended(lease).await;
            sessions.remove(&session_id_bytes).await;
        }
        RelayOutcome::DataStreamDied { tcp_read, tcp_write } => {
            // まだ`Established`のまま(=fencing slotを保持したまま)にする —
            // 一致する`RESUME`がこのsession_idの元へ戻ってこられるように。
            let mut session = handle.lock().await;
            session.parked_tcp = Some((tcp_read, tcp_write));
            session.parked_since = Some(std::time::Instant::now());
        }
    }
    Ok(())
}

/// Phase 8-3 / quicmux-server-resume Stage B: `quicmux::resume`のRESUMEフレーム
/// (frame typeバイトは呼び出し元`handle_connection`が既に読み取り済み)を
/// 検証し、既存セッションに park された TCP 接続を取り戻して中継を再開する。
/// `token`=session_id・`auth_blob`=resume proof はquicmuxにとって完全に
/// opaqueなbytesなので、その意味付け(HMAC検証・SessionTable/AttachRuntime
/// との突き合わせ)はすべてこの関数(呼び出し側)の責務になる —
/// `quicmux::resume`モジュールdocsの「ResumeAcceptorが担う一点」そのもの
/// (ただしここでは`dyn ResumeAcceptor`を経由せず、`decode_resume_request`/
/// `respond_resume_*`を直接呼んでいる。理由: `handle_connection`は既に
/// ATTACH_HELLO/CancelAttachと同じ一本のstreamの先頭1byteで種別分岐して
/// いるため、`accept_resume`のように新規`accept_bi()`を自前で行う版は
/// 使えない)。
async fn handle_resume_stream(
    conn: AnyMuxConnection,
    mut send: AnyByteStreamWriteHalf,
    mut recv: AnyByteStreamReadHalf,
    target: SocketAddr,
    session_secret: [u8; 32],
    attach_runtime: Arc<AttachRuntime>,
    sessions: SessionTable,
) -> Result<()> {
    // resume_proof = HMAC(session_secret, exporter(新connection) || session_id)
    // （HELPER_PROTOCOL.md §7.3。session_id を混ぜることで、同じ session_secret を
    // 使い回す複数セッションが互いの resume トークンを流用できないようにする）。
    let exporter = conn
        .export_keying_material(EXPORTER_LABEL, b"")
        .await
        .map_err(|e| anyhow!("export_keying_material failed: {e:?}"))?;
    let request = quicmux::decode_resume_request(&mut recv, exporter).await.context("failed to decode RESUME frame")?;

    let session_id: [u8; 16] = request.token.as_slice().try_into().map_err(|_| {
        anyhow!("resume token has unexpected length {} (expected 16)", request.token.len())
    })?;
    // quicmuxはauth_blobの中身を一切解釈しない — このHMAC検証はisekai-pipe
    // 独自のポリシーとしてここで行う(`quicmux::resume`モジュールdocs参照)。
    let mut mac = HmacSha256::new_from_slice(&session_secret).expect("HMAC accepts any key length");
    mac.update(&exporter);
    mac.update(&session_id);
    let expected = mac.finalize().into_bytes();
    if request.auth_blob.ct_eq(expected.as_slice()).unwrap_u8() != 1 {
        quicmux::respond_resume_rejected(&mut send, quicmux::ResumeRejectReason::Auth).await;
        return Err(anyhow!("resume proof mismatch, rejecting"));
    }
    let client_delivered_offset = request.client_delivered_offset;

    let Some(handle) = sessions.get(&session_id).await else {
        quicmux::respond_resume_rejected(&mut send, quicmux::ResumeRejectReason::UnknownToken).await;
        return Err(anyhow!(
            "unknown session_id for resume: {}",
            hex_lower(&session_id)
        ));
    };

    let parked = {
        let mut session = handle.lock().await;
        session.parked_since = None;
        session.parked_tcp.take()
    };
    let Some((tcp_read, tcp_write)) = parked else {
        quicmux::respond_resume_rejected(&mut send, quicmux::ResumeRejectReason::UnknownToken).await;
        return Err(anyhow!(
            "session {} not resumable (no parked TCP connection)",
            hex_lower(&session_id)
        ));
    };

    // `RESUME`は`ATTACH_HELLO`のfencing判定(`AttachRuntime::hello`)を経由
    // しない — この`session_id`が現在まさに`Established`スロットを占有して
    // いる、その`lease`を確認するだけでよい(module docs: 同一sessionへの
    // resumeはfencing上の競合になり得ない)。
    let Some(lease) = attach_runtime.established_lease_for(isekai_protocol::SessionId::from_bytes(session_id)).await
    else {
        repark(&handle, tcp_read, tcp_write).await;
        quicmux::respond_resume_rejected(&mut send, quicmux::ResumeRejectReason::UnknownToken).await;
        return Err(anyhow!("no established attach slot for session {}", hex_lower(&session_id)));
    };

    let (helper_committed_offset, helper_sent_offset, replay_bytes) = {
        let session = handle.lock().await;
        let replay = session.output_buffer.replay_from(client_delivered_offset);
        (
            session.helper_committed_offset,
            session.output_buffer.end_offset(),
            replay,
        )
    };
    let Some(replay_bytes) = replay_bytes else {
        repark(&handle, tcp_read, tcp_write).await;
        quicmux::respond_resume_rejected(&mut send, quicmux::ResumeRejectReason::OffsetGone).await;
        return Err(anyhow!(
            "requested offset {client_delivered_offset} no longer in output buffer for session {}",
            hex_lower(&session_id)
        ));
    };

    log::info!(
        "resume: session_id={} client_sent_offset={} client_delivered_offset={client_delivered_offset} \
         helper_committed_offset={helper_committed_offset} replaying {} bytes",
        hex_lower(&session_id),
        request.client_sent_offset,
        replay_bytes.len()
    );

    if let Err(e) = quicmux::respond_resume_accepted(&mut send, helper_committed_offset, helper_sent_offset, &replay_bytes).await {
        repark(&handle, tcp_read, tcp_write).await;
        return Err(anyhow!("failed to write RESUME_ACK: {e}"));
    }

    // control stream も新しい connection 上で作り直す（元の control stream は
    // 古い connection に紐づいたまま失効している）。8-1 と同じ理由で、
    // 確立を待たずに中継を先に始める。既知のsession_idをそのまま再利用する
    // (#18-4: 新規発行はしない)。
    let control_task = {
        let conn = conn.clone();
        let handle = handle.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(HELLO_TIMEOUT, accept_control_stream(&conn, session_secret, session_id)).await
            {
                Ok(Ok((csend, crecv))) => {
                    log::info!("resume: control stream re-established for session_id={}", hex_lower(&session_id));
                    spawn_app_ack_tasks(csend, crecv, handle);
                }
                Ok(Err(e)) => log::info!("resume: control stream re-establish failed ({e:#})"),
                Err(_) => log::info!("resume: control stream not re-opened within timeout"),
            }
        })
    };

    let outcome = relay_buffered(
        &mut send,
        &mut recv,
        tcp_read,
        tcp_write,
        handle.clone(),
        target,
    )
    .await;
    control_task.abort();
    finish_or_park_session(&sessions, &attach_runtime, lease, Some(session_id), handle, outcome).await;
    Ok(())
}

/// `handle_resume_stream` の各早期リターン経路で共通の後始末: 取り出した
/// TCP 接続をもう一度 `Session::parked_tcp` に戻し、`parked_since` を今の
/// 時刻に更新する（次に来る resume 試行、またはタイムアウトでの破棄に備える）。
async fn repark(
    handle: &Arc<Mutex<Session>>,
    tcp_read: tokio::net::tcp::OwnedReadHalf,
    tcp_write: tokio::net::tcp::OwnedWriteHalf,
) {
    let mut session = handle.lock().await;
    session.parked_tcp = Some((tcp_read, tcp_write));
    session.parked_since = Some(std::time::Instant::now());
}

/// クライアントが`HELLO`で希望したresume-grace期間を、このサーバー自身の
/// `--resume-window`(`max_resume_grace_secs`、実際にsessionをparkし続ける上限)で
/// clampする。`requested == 0`は「希望なし」を意味し、その場合はこのサーバーの
/// 上限をそのまま実効値として使う——クライアント側の設定だけでサーバー上の
/// session保持時間(≒リソース消費)を際限なく増やせる設計にしないための境界
/// (ISEKAI_PIPE_DESIGN.md)。
fn effective_resume_grace(requested_resume_grace_secs: u32, max_resume_grace_secs: u64) -> u32 {
    let max = u32::try_from(max_resume_grace_secs).unwrap_or(u32::MAX);
    if requested_resume_grace_secs == 0 {
        max
    } else {
        requested_resume_grace_secs.min(max)
    }
}

/// `session_secret` と QUIC connection の exporter から proof を計算する
/// （data stream HELLO と control stream CONTROL_HELLO で共通のロジック）。
/// `quicmux::AnyMuxConnection::export_keying_material`が非同期(qmuxバックエンドは
/// 一度captureした値を返すだけだが、noqバックエンドは呼び出し毎に計算するため
/// 将来的な非同期化に備えて両方ともasync)になったため、この関数もasyncにした。
async fn compute_proof(
    conn: &AnyMuxConnection,
    session_secret: &[u8; 32],
    label: &[u8],
    context: &[u8],
) -> Result<[u8; 32]> {
    let exporter = conn
        .export_keying_material(label, context)
        .await
        .map_err(|e| anyhow!("export_keying_material failed: {e:?}"))?;
    let mut mac = HmacSha256::new_from_slice(session_secret).expect("HMAC accepts any key length");
    mac.update(&exporter);
    Ok(mac.finalize().into_bytes().into())
}

/// `ATTACH_HELLO`/`CancelAttach`のproof計算(`#18`): 通常の`compute_proof`と
/// 同じexporterを使うが、`isekai_transport::proof::compute_proof`の`extra`
/// パラメータと対称になるよう、`transcript`(`attach_hello_proof_transcript`
/// 等が返すbyte列)をHMACに追加で混ぜ込む。
async fn compute_attach_proof(conn: &AnyMuxConnection, session_secret: &[u8; 32], transcript: &[u8]) -> Result<[u8; 32]> {
    let exporter = conn
        .export_keying_material(EXPORTER_LABEL, b"")
        .await
        .map_err(|e| anyhow!("export_keying_material failed: {e:?}"))?;
    let mut mac = HmacSha256::new_from_slice(session_secret).expect("HMAC accepts any key length");
    mac.update(&exporter);
    mac.update(transcript);
    Ok(mac.finalize().into_bytes().into())
}

/// control stream を accept し、`CONTROL_HELLO` を検証して`CONTROL_ACK`を返す。
/// `session_id`は(#18-4以降)クライアントが`ATTACH_HELLO`で既に決めているため、
/// ここでは新規発行せず、そのままecho backするだけ。Phase 8 未対応の
/// client（control stream を開かない）向けに呼び出し側でタイムアウトを
/// 掛けることを想定している。
async fn accept_control_stream(
    conn: &AnyMuxConnection,
    session_secret: [u8; 32],
    session_id: resume::SessionId,
) -> Result<(AnyByteStreamWriteHalf, AnyByteStreamReadHalf)> {
    let stream = conn.accept_bi().await.context("no control stream opened")?;
    let (mut crecv, mut csend) = stream.split();
    let mut hello = [0u8; 33];
    read_exact(&mut crecv, &mut hello)
        .await
        .context("failed to read CONTROL_HELLO")?;
    if hello[0] != resume::CONTROL_HELLO {
        return Err(anyhow!("unexpected control frame type: {:#x}", hello[0]));
    }
    let expected = compute_proof(conn, &session_secret, EXPORTER_LABEL, b"").await?;
    if hello[1..33].ct_eq(&expected).unwrap_u8() != 1 {
        return Err(anyhow!("CONTROL_HELLO proof mismatch"));
    }

    let mut ack = Vec::with_capacity(17);
    ack.push(resume::CONTROL_ACK);
    ack.extend_from_slice(&session_id);
    csend
        .write_all(&ack)
        .await
        .context("failed to write CONTROL_ACK")?;

    Ok((csend, crecv))
}

/// data stream 側の中継ループ終了後、TCP 接続がまだ生きているとみなせるなら
/// `Session::parked_tcp` に戻して resume を待てるようにし(`AttachArbiter`は
/// `Established`のまま、`lease`もfencing slotを占有し続ける)、TCP 自体が
/// 死んでいるなら`AttachRuntime::relay_ended`でslotを解放しテーブルから
/// 破棄する。`id`が`None`なのは起こり得ない(#18-4以降、session_idは常に
/// `ATTACH_HELLO`の時点で存在する)が、呼び出し側の型を単純に保つため
/// `Option`のまま残す。
async fn finish_or_park_session(
    sessions: &SessionTable,
    attach_runtime: &Arc<AttachRuntime>,
    lease: attach_arbiter::LeaseId,
    id: Option<resume::SessionId>,
    handle: Arc<Mutex<Session>>,
    outcome: RelayOutcome,
) {
    let Some(id) = id else {
        return;
    };
    match outcome {
        RelayOutcome::TcpDied => {
            log::info!(
                "session {} target connection died, discarding",
                hex_lower(&id)
            );
            attach_runtime.relay_ended(lease).await;
            sessions.remove(&id).await;
        }
        RelayOutcome::DataStreamDied {
            tcp_read,
            tcp_write,
        } => {
            log::info!(
                "session {} data stream died, parking for possible resume",
                hex_lower(&id)
            );
            let mut session = handle.lock().await;
            session.parked_tcp = Some((tcp_read, tcp_write));
            session.parked_since = Some(std::time::Instant::now());
            // sessions テーブルには既に insert_existing 済みなのでそのまま残す。
            // `attach_runtime`はEstablishedのまま(=fencing slotを保持)にする。
        }
    }
}

/// APP_ACK の送受信を行う背後タスクを spawn する。data stream 側が終わって
/// `relay_with_resume` が呼び出し元の control_task を abort() すれば、
/// control stream 側の read/write もいずれエラーになりループを抜ける。
fn spawn_app_ack_tasks(
    mut csend: AnyByteStreamWriteHalf,
    mut crecv: AnyByteStreamReadHalf,
    session: Arc<Mutex<Session>>,
) {
    // APP_ACK 受信: client からの client_delivered_offset を受け取り、
    // output buffer の破棄範囲を進める。
    {
        let session = session.clone();
        tokio::spawn(async move {
            loop {
                let mut frame = [0u8; 9];
                match read_exact(&mut crecv, &mut frame).await {
                    Ok(()) if frame[0] == resume::APP_ACK => {
                        let offset = u64::from_be_bytes(frame[1..9].try_into().unwrap());
                        let notify = {
                            let mut session = session.lock().await;
                            let was_full = session.output_buffer.is_full();
                            session.output_buffer.advance_start(offset);
                            if was_full && !session.output_buffer.is_full() {
                                Some(session.output_space_available.clone())
                            } else {
                                None
                            }
                        };
                        if let Some(notify) = notify {
                            notify.notify_waiters();
                        }
                    }
                    _ => break,
                }
            }
        });
    }

    // APP_ACK 送信: helper_committed_offset（C→S の受信確認）を 200ms ごとに
    // control stream 経由で client に送る（進みが無ければ送らない）。
    tokio::spawn(async move {
        let mut last_sent = 0u64;
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let current = session.lock().await.helper_committed_offset;
            if current == last_sent {
                continue;
            }
            let mut frame = Vec::with_capacity(9);
            frame.push(resume::APP_ACK);
            frame.extend_from_slice(&current.to_be_bytes());
            if csend.write_all(&frame).await.is_err() {
                break;
            }
            last_sent = current;
        }
    });
}

/// `relay_buffered` の終了理由。呼び出し側はこれを見て、TCP 接続を
/// `Session::parked_tcp` に戻して resume を待つか、破棄するかを決める。
enum RelayOutcome {
    /// target への TCP 接続自体が終わった（相手が正常/異常終了）。
    /// resume する意味が無いので session ごと破棄してよい。
    TcpDied,
    /// data stream（QUIC）側が終わった。TCP 接続はまだ生きているとみなせるので、
    /// 呼び出し側に返す。resume を待つために park できる。
    DataStreamDied {
        tcp_read: tokio::net::tcp::OwnedReadHalf,
        tcp_write: tokio::net::tcp::OwnedWriteHalf,
    },
}

/// output buffer 付きの中継。S→C 方向は `Session::output_buffer` に tee しつつ
/// 送出し、C→S 方向は `Session::helper_committed_offset` を進める。
/// control stream が最終的に確立しなかった場合でも、この関数自体は
/// Phase 7 と同じ双方向コピーとして機能する（バッファへの tee はしているが
/// 誰も参照しないだけで、実害はない。上限付きなので無制限には増えない）。
///
/// `tokio::join!` で両方向を独立に完了させる設計だと、片方向だけが
/// data stream 側のエラーで終わり、もう片方向が TCP からの次のデータを
/// 待ったまま（sshd がしばらく何も出力しない等）永久にブロックし得る
/// バグがあったため、単一の `tokio::select!` ループに書き直した。
/// いずれかの方向が「これ以上続けられない」と判断した時点で即座に終了する。
async fn relay_buffered(
    send: &mut AnyByteStreamWriteHalf,
    recv: &mut AnyByteStreamReadHalf,
    mut tcp_read: tokio::net::tcp::OwnedReadHalf,
    mut tcp_write: tokio::net::tcp::OwnedWriteHalf,
    session: Arc<Mutex<Session>>,
    target: SocketAddr,
) -> RelayOutcome {
    let mut c2s_buf = vec![0u8; 16 * 1024];
    let mut s2c_buf = vec![0u8; 16 * 1024];
    let mut c2s_done = false; // client → helper 方向が half-close 済み
    let output_space_available = session.lock().await.output_space_available.clone();

    loop {
        let s2c_read_len = {
            let session = session.lock().await;
            session
                .output_buffer
                .remaining_capacity()
                .min(s2c_buf.len())
        };
        tokio::select! {
            // `AnyByteStreamReadHalf::read`は`tokio::io::AsyncRead`と同じ規約
            // (`Ok(0)` = EOF)であり、旧`noq::RecvStream::read`の`Ok(None)` = EOF
            // (`Ok(Some(n))` = n>0バイト)とは異なるため、マッチの形を合わせて
            // 移植した。
            result = recv.read(&mut c2s_buf), if !c2s_done => {
                match result {
                    Ok(n) if n > 0 => {
                        if let Err(e) = tcp_write.write_all(&c2s_buf[..n]).await {
                            log::warn!("relay to {target}: tcp write failed: {e}");
                            return RelayOutcome::TcpDied;
                        }
                        session.lock().await.helper_committed_offset += n as u64;
                    }
                    Ok(_) => {
                        // client 側の half-close。S→C 方向はまだ継続する。
                        let _ = tcp_write.shutdown().await;
                        c2s_done = true;
                    }
                    Err(e) => {
                        log::info!("relay to {target}: data stream (C->S) ended: {e}");
                        return RelayOutcome::DataStreamDied { tcp_read, tcp_write };
                    }
                }
            }
            _ = output_space_available.notified(), if s2c_read_len == 0 => {
                continue;
            }
            _ = tokio::time::sleep(Duration::from_millis(50)), if s2c_read_len == 0 => {
                continue;
            }
            result = tcp_read.read(&mut s2c_buf[..s2c_read_len]), if s2c_read_len > 0 => {
                match result {
                    Ok(0) => {
                        // target（sshd）側が正常終了。resume する意味が無い。
                        log::info!("relay to {target}: tcp closed cleanly");
                        let _ = send.shutdown().await;
                        return RelayOutcome::TcpDied;
                    }
                    Ok(n) => {
                        if let Err(e) = send.write_all(&s2c_buf[..n]).await {
                            log::info!("relay to {target}: data stream (S->C) write failed: {e}");
                            return RelayOutcome::DataStreamDied { tcp_read, tcp_write };
                        }
                        if !session.lock().await.output_buffer.append(&s2c_buf[..n]) {
                            log::warn!(
                                "relay to {target}: output buffer had no room after bounded read; treating as data stream failure"
                            );
                            return RelayOutcome::DataStreamDied { tcp_read, tcp_write };
                        }
                    }
                    Err(e) => {
                        log::warn!("relay to {target}: tcp read failed: {e}");
                        return RelayOutcome::TcpDied;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod bootstrap_request_tests {
    use super::*;
    use isekai_protocol::BootstrapCandidateV2;

    fn sample_request(client_candidates: Vec<BootstrapCandidateV2>) -> isekai_protocol::BootstrapRequestV2 {
        isekai_protocol::BootstrapRequestV2 {
            v: isekai_protocol::BOOTSTRAP_PROTOCOL_V2,
            session_id: "00".repeat(16),
            bootstrap_attempt_id: "11".repeat(16),
            client_candidates,
        }
    }

    #[test]
    fn resolve_bootstrap_request_reads_validates_and_unlinks_the_file() {
        let request = sample_request(vec![]);
        let json = serde_json::to_vec(&request).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bootstrap-request.json");
        std::fs::write(&path, &json).unwrap();

        let decoded = resolve_bootstrap_request(path.to_str().unwrap()).unwrap();
        assert_eq!(decoded, request);
        assert!(!path.exists(), "file should be unlinked after a successful read");
    }

    #[test]
    fn resolve_bootstrap_request_rejects_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bootstrap-request.json");
        std::fs::write(&path, b"not json").unwrap();

        let err = resolve_bootstrap_request(path.to_str().unwrap());
        assert!(err.is_err());
    }

    #[test]
    fn resolve_bootstrap_request_errors_on_missing_file() {
        let err = resolve_bootstrap_request("/nonexistent/isekai-bootstrap-request.json");
        assert!(err.is_err());
    }

    #[test]
    fn client_candidate_punch_targets_parses_valid_endpoints() {
        let request = sample_request(vec![
            BootstrapCandidateV2 {
                route: "stun-p2p".to_string(),
                endpoint: "203.0.113.5:4000".to_string(),
                valid_for_ms: 5_000,
            },
            BootstrapCandidateV2 {
                route: "stun-p2p".to_string(),
                endpoint: "203.0.113.6:4001".to_string(),
                valid_for_ms: 5_000,
            },
        ]);

        let targets = client_candidate_punch_targets(&request);
        assert_eq!(
            targets,
            vec![
                "203.0.113.5:4000".parse::<SocketAddr>().unwrap(),
                "203.0.113.6:4001".parse::<SocketAddr>().unwrap(),
            ]
        );
    }

    #[test]
    fn client_candidate_punch_targets_is_empty_for_no_candidates() {
        let request = sample_request(vec![]);
        assert!(client_candidate_punch_targets(&request).is_empty());
    }
}

#[cfg(test)]
mod relay_transport_tests {
    use super::*;

    fn args_with(extra: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = vec![
            "--relay", "203.0.113.1:4433", "--relay-sni", "relay.test", "--relay-jwt", "test-jwt",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    }

    #[test]
    fn relay_transport_defaults_to_udp() {
        let args = parse_args_from(args_with(&[])).unwrap();
        assert_eq!(args.relay_transport, RelayTransportKind::Udp);
    }

    #[test]
    fn relay_transport_qmux_parses() {
        let args = parse_args_from(args_with(&["--relay-transport", "qmux"])).unwrap();
        assert_eq!(args.relay_transport, RelayTransportKind::Qmux);
    }

    #[test]
    fn relay_transport_rejects_unknown_value() {
        let err = parse_args_from(args_with(&["--relay-transport", "bogus"]));
        assert!(err.is_err());
    }

    #[test]
    fn relay_transport_requires_relay() {
        let args: Vec<String> = vec!["--relay-transport", "qmux"].into_iter().map(String::from).collect();
        let err = parse_args_from(args);
        assert!(err.is_err(), "--relay-transport without --relay should be rejected");
    }
}

#[cfg(test)]
mod bind_port_range_tests {
    use super::*;

    #[test]
    fn parses_a_valid_range() {
        assert_eq!(parse_bind_port_range("40000-40100").unwrap(), (40000, 40100));
    }

    #[test]
    fn accepts_a_single_port_range() {
        assert_eq!(parse_bind_port_range("40000-40000").unwrap(), (40000, 40000));
    }

    #[test]
    fn rejects_start_greater_than_end() {
        assert!(parse_bind_port_range("40100-40000").is_err());
    }

    #[test]
    fn rejects_missing_separator() {
        assert!(parse_bind_port_range("40000").is_err());
    }

    #[test]
    fn rejects_non_numeric_bounds() {
        assert!(parse_bind_port_range("abc-def").is_err());
    }

    #[test]
    fn cli_flag_sets_bind_port_range() {
        let args = parse_args_from(
            ["--bind", "0.0.0.0:0", "--bind-port-range", "40000-40010"].into_iter().map(String::from),
        )
        .unwrap();
        assert_eq!(args.bind_port_range, Some((40000, 40010)));
    }

    #[test]
    fn cli_flag_rejects_an_explicit_nonzero_bind_port() {
        let err = parse_args_from(
            ["--bind", "0.0.0.0:2222", "--bind-port-range", "40000-40010"].into_iter().map(String::from),
        );
        assert!(err.is_err(), "--bind-port-range with a pinned --bind port should be rejected");
    }

    #[test]
    fn bind_udp_socket_without_a_range_uses_binds_own_port() {
        let socket = bind_udp_socket("127.0.0.1:0".parse().unwrap(), None).unwrap();
        assert_ne!(socket.local_addr().unwrap().port(), 0);
    }

    #[test]
    fn bind_udp_socket_with_a_range_picks_a_port_inside_it() {
        let socket = bind_udp_socket("127.0.0.1:0".parse().unwrap(), Some((40000, 40100))).unwrap();
        let port = socket.local_addr().unwrap().port();
        assert!((40000..=40100).contains(&port), "port {port} outside requested range");
    }

    #[test]
    fn bind_udp_socket_with_a_single_port_range_binds_exactly_that_port() {
        // Bind once to grab a free ephemeral port, release it, then ask
        // `bind_udp_socket` for that exact single-port range — proof the
        // range is actually honored, not ignored (a bug that always fell
        // through to an OS-assigned port would still pass a looser test).
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let free_port = probe.local_addr().unwrap().port();
        drop(probe);
        let socket = bind_udp_socket("127.0.0.1:0".parse().unwrap(), Some((free_port, free_port))).unwrap();
        assert_eq!(socket.local_addr().unwrap().port(), free_port);
    }

    #[test]
    fn bind_udp_socket_skips_an_already_bound_port_within_the_range() {
        // A window of just `held_port..=held_port+1` is too narrow to be
        // reliable under concurrent test execution: Windows's ephemeral port
        // allocator hands out ports sequentially (unlike Linux's more
        // randomized selection), so with many tests binding ephemeral ports
        // concurrently in the same process, `held_port + 1` is itself
        // frequently already taken by some unrelated concurrent bind,
        // failing this test for a reason unrelated to what it's actually
        // testing (found via a real `test-windows` CI failure). A wider
        // window keeps the same intent (assert the returned port isn't the
        // one we're holding) while giving `bind_udp_socket` many more chances
        // to find a free port despite that noise.
        let held = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let held_port = held.local_addr().unwrap().port();
        let socket =
            bind_udp_socket("127.0.0.1:0".parse().unwrap(), Some((held_port, held_port.saturating_add(31)))).unwrap();
        assert_ne!(socket.local_addr().unwrap().port(), held_port);
    }
}
