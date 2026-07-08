mod plain_socket;
mod resume;

use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use noq::crypto::rustls::QuicServerConfig;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use resume::{Session, SessionTable};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

// isekai-helper: 認証付き QUIC↔TCP リレー。
// 契約の詳細は /HELPER_PROTOCOL.md を参照。このファイルはその契約の実装。

type HmacSha256 = Hmac<Sha256>;

/// S→C output buffer の既定上限（HELPER_PROTOCOL.md §7.4 の既定案）。
const DEFAULT_RESUME_BUFFER_SIZE: usize = 4 * 1024 * 1024;

/// `SessionTable` に同時保持できるセッション数の既定上限（Phase S-4b）。
/// 通常運用でこれだけ同時に resume 待ちセッションが積まれることは想定しにくい
/// ため、小さめの値にして DoS/リソース枯渇対策を優先する。
const DEFAULT_MAX_SESSIONS: usize = 16;

const EXPORTER_LABEL: &[u8] = b"isekai-pipe-auth-v1";
const ALPN: &[u8] = b"isekai-pipe/1";

const FRAME_HELLO: u8 = 0x01;
const FRAME_ACK: u8 = 0x02;
const FRAME_REJECT_TARGET: u8 = 0xFC;
const FRAME_REJECT_UNSUPPORTED: u8 = 0xFD;
const FRAME_REJECT_DUPLICATE: u8 = 0xFE;
const FRAME_REJECT_AUTH: u8 = 0xFF;

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

struct Args {
    target: SocketAddr,
    service_name: String,
    bind: SocketAddr,
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
    /// セキュリティレビュー #58: argv経由(`--relay-jwt`)は他のローカルユーザーから
    /// `ps aux`/`/proc/<pid>/cmdline`で読める。後方互換のため引数自体は残すが、
    /// 実際のブートストラップ呼び出し元(`helper_bootstrap.rs`/`isekai-bootstrap::openssh`)
    /// は全て`relay_jwt_file`(ファイル経由)に切り替え済み。
    relay_jwt: Option<String>,
    /// `--relay-jwt`の推奨代替。ファイルパスを受け取り、起動時に一度だけ読み取ってから
    /// 直ちに内容をゼロクリアしunlinkする(`resolve_relay_jwt`参照)。
    relay_jwt_file: Option<String>,
}

fn next_val(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    iter.next()
        .ok_or_else(|| anyhow!("{flag} requires a value"))
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
    println!("    --idle-timeout <SECS>          QUIC transport idle timeout (default: 15)");
    println!("    --resume-window <SECS>         how long a parked (disconnected) session stays resumable (default: 120)");
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
        "    --relay-jwt-file <PATH>        path to a file containing the Bearer token for --relay"
    );
    println!("                                   (preferred: unlike --relay-jwt, never appears in");
    println!("                                   `ps`/`/proc/<pid>/cmdline`; read once at startup and removed)");
    println!("    --relay-jwt <TOKEN>            Bearer token to authenticate to --relay (deprecated: visible");
    println!(
        "                                   to other local users via `ps`/`/proc/<pid>/cmdline`;"
    );
    println!("                                   prefer --relay-jwt-file)");
    println!("    --log-level <LEVEL>            error|warn|info|debug|trace (default: info)");
    println!("    --version                      print version and exit");
    println!("    -h, --help                     print this help message");
}

fn parse_args_from(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut target: SocketAddr = "127.0.0.1:22".parse().unwrap();
    let mut service_name = "ssh".to_string();
    let mut bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let mut idle_timeout = 15u64;
    // 実機検証（Phase 8-4b）で、reattach が5回とも失敗する最悪ケースは
    // 「各試行の QUIC handshake タイムアウト（`--idle-timeout` と同じ 15秒）」×5回 +
    // 指数バックオフ合計15秒 で実測 約90秒かかることを確認した。90秒ちょうどだと
    // ギリギリなので余裕を持たせて120秒にしてある。
    let mut resume_window = 120u64;
    let mut resume_buffer_size = DEFAULT_RESUME_BUFFER_SIZE;
    let mut max_idle_lifetime = 600u64;
    let mut max_sessions = DEFAULT_MAX_SESSIONS;
    let mut once = false;
    let mut log_level = "info".to_string();
    let mut stun_server: Option<SocketAddr> = None;
    let mut punch_peer: Option<SocketAddr> = None;
    let mut relay: Option<SocketAddr> = None;
    let mut relay_sni: Option<String> = None;
    let mut relay_jwt: Option<String> = None;
    let mut relay_jwt_file: Option<String> = None;

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
            "--relay-jwt" => relay_jwt = Some(next_val(&mut iter, "--relay-jwt")?),
            "--relay-jwt-file" => relay_jwt_file = Some(next_val(&mut iter, "--relay-jwt-file")?),
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
    if service_name.is_empty() {
        return Err(anyhow!("--service-name must not be empty"));
    }
    Ok(Args {
        target,
        service_name,
        bind,
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
        relay_jwt,
        relay_jwt_file,
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

    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;
    server_crypto.alpn_protocols = vec![ALPN.to_vec()];
    // 0-RTT / early data はクライアント・サーバー双方で無効化する契約（HELPER_PROTOCOL.md 参照）。
    // rustls は max_early_data_size を明示的に増やさない限り 0-RTT を送出しないが、契約として明示する。
    server_crypto.max_early_data_size = 0;

    let idle_timeout_cfg = noq::IdleTimeout::try_from(Duration::from_secs(args.idle_timeout))
        .map_err(|_| anyhow!("invalid --idle-timeout"))?;
    let keep_alive = Duration::from_secs((args.idle_timeout / 3).max(1));

    let mut transport = noq::TransportConfig::default();
    transport.max_idle_timeout(Some(idle_timeout_cfg));
    transport.keep_alive_interval(Some(keep_alive));
    // data stream（Phase 7）+ control stream（Phase 8、resume 用）の 2 本を許可する
    // （HELPER_PROTOCOL.md §7.1）。3 本目以降は Phase 7 と同様 reset される。
    transport.max_concurrent_bidi_streams(noq::VarInt::from_u32(2));
    transport.max_concurrent_uni_streams(noq::VarInt::from_u32(0));
    transport.datagram_receive_buffer_size(None);
    // Phase 9-1: multipath 対応。既存 quinn クライアント（Phase 7/8）は
    // open_path() を呼ばないため path0 のみで従来通り動作し、後方互換に
    // 影響しない（Phase 9-0 の compat_check.rs で実証済み）。
    transport.max_concurrent_multipath_paths(8);

    let mut server_config =
        noq::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_crypto)?));
    server_config.transport_config(Arc::new(transport));
    // preferred_address は明示的に設定しない（QUIC-Exfil 対策、既定で未使用）。

    let (endpoint, stun_observed_addr, relay_public_addr) = if let Some(relay_addr) = args.relay {
        // relay版P2P(TransportPreference::IsekaiLinkRelayQuic): 自前でbindする代わりに
        // MASQUE relayへCONNECT-UDP-bindトンネルを張り、relayが割り当てた公開アドレスを
        // isekai-terminal側へ(SSHブートストラップのハンドシェイクJSON経由で)伝える。
        // isekai-terminal自身はrelay/MASQUEを一切意識せず、この公開アドレスへ普通にQUIC
        // 接続するだけでよい(isekai_link_relay_transport.rs参照)。
        let relay_sni = args.relay_sni.expect("validated in parse_args");
        let relay_jwt = resolve_relay_jwt(args.relay_jwt, args.relay_jwt_file)?;
        let (relay_socket, proxy_public_address) =
            isekai_link_masque::connect_relay_agent(relay_addr, &relay_sni, &relay_jwt)
                .await
                .map_err(|e| anyhow!("relay connect failed: {e}"))?;
        log::info!("relay: tunnel established, proxy_public_address={proxy_public_address}");
        let endpoint = noq::Endpoint::new_with_abstract_socket(
            noq::EndpointConfig::default(),
            Some(server_config),
            Box::new(relay_socket),
            Arc::new(noq::TokioRuntime),
        )?;
        (endpoint, None, Some(proxy_public_address))
    } else {
        // 自前でbindしたソケットを、noqへ渡す前にSTUN問い合わせ・穴あけprobeへ使う
        // （bind_faulty_udp_socket的なラップをする前の生ソケットとして扱う唯一の機会 ——
        // 一度 noq::Endpoint に渡すと、以後の recv は全て noq 自身の poll_recv が
        // 消費してしまい、こちらから直接 recv_from で読むと競合する）。
        let std_socket = std::net::UdpSocket::bind(args.bind)?;
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

        if let Some(peer) = args.punch_peer {
            log::info!("punch: sending hole-punch probes to {peer}");
            // 中身はNAT越え専用のマーカーで構わない(相手はQUICパケットとして解釈できない
            // 限り黙って破棄するだけであり、応答は期待しない・待たない)。
            for _ in 0..5 {
                let _ = raw_socket.send_to(b"isekai-punch", peer).await;
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }

        let endpoint = noq::Endpoint::new_with_abstract_socket(
            noq::EndpointConfig::default(),
            Some(server_config),
            Box::new(plain_socket::PlainUdpSocket::new(raw_socket)),
            Arc::new(noq::TokioRuntime),
        )?;
        (endpoint, stun_observed_addr, None)
    };
    let listen_port = endpoint.local_addr()?.port();
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
    {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "{handshake}").context("failed to write handshake to stdout")?;
        stdout.flush().context("failed to flush stdout handshake")?;
    }

    log::info!(
        "isekai-helper listening on udp/{} (target={}, cert_sha256={})",
        endpoint.local_addr()?,
        args.target,
        cert_sha256
    );

    let active = Arc::new(AtomicBool::new(false));
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
    // 十分長い既定値（90秒）にしてある。
    {
        let sessions = sessions.clone();
        let max_parked = Duration::from_secs(args.resume_window);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                sessions.sweep_expired_parked(max_parked).await;
            }
        });
    }

    // --max-idle-lifetime の監視タスク。
    // アクティブな接続が無く、かつ最後の接続終了（または起動）からこの秒数が経過したら自己終了する。
    {
        let active = active.clone();
        let last_activity = last_activity.clone();
        let max_idle = Duration::from_secs(args.max_idle_lifetime);
        let idle_shutdown = idle_shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if active.load(Ordering::SeqCst) {
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
                endpoint.close(0u32.into(), b"shutdown");
                break;
            }
            _ = idle_shutdown.notified() => {
                log::info!("max-idle-lifetime reached, closing endpoint");
                endpoint.close(0u32.into(), b"idle-timeout");
                break;
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let target = args.target;
                let secret = session_secret;
                let active = active.clone();
                let last_activity = last_activity.clone();
                let sessions = sessions.clone();
                let resume_buffer_size = args.resume_buffer_size;
                let max_resume_grace_secs = args.resume_window;
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            // noq: `remote_address()`はConnectingにしか無い（確立後はpath0/1...
                            // それぞれに別アドレスがあり得るためmultipath化で無くなった）。
                            // path0（PathId::ZERO）は常に存在するのでログ用途にはこれで十分。
                            let remote = conn
                                .path(noq::PathId::ZERO)
                                .and_then(|p| p.remote_address().ok());
                            log::info!("QUIC connection established from {remote:?}");
                            if let Err(e) = handle_connection(conn, target, secret, active, sessions, resume_buffer_size, max_resume_grace_secs).await {
                                log::warn!("connection from {remote:?} ended: {e:#}");
                            }
                        }
                        Err(e) => log::warn!("failed to accept connection: {e:#}"),
                    }
                    *last_activity.lock().await = Instant::now();
                });
                if once {
                    endpoint.close(0u32.into(), b"once");
                    break;
                }
            }
        }
    }

    endpoint.wait_idle().await;
    Ok(())
}

async fn handle_connection(
    conn: noq::Connection,
    target: SocketAddr,
    session_secret: [u8; 32],
    active: Arc<AtomicBool>,
    sessions: SessionTable,
    resume_buffer_size: usize,
    max_resume_grace_secs: u64,
) -> Result<()> {
    // 最初の1バイトでフレーム種別（HELLO=新規 / RESUME=reattach）を判定してから、
    // 種別に応じた残りバイト数を読む。いずれも一定時間内に届かなければ
    // connection を close する（QUIC connection だけ張って stream を開かない
    // 妨害を防ぐ）。
    let (send, recv, frame_type, rest) = tokio::time::timeout(HELLO_TIMEOUT, async {
        let (send, mut recv) = conn.accept_bi().await.context("no stream opened")?;
        let mut type_byte = [0u8; 1];
        recv.read_exact(&mut type_byte)
            .await
            .context("failed to read frame type")?;
        let rest_len = match type_byte[0] {
            FRAME_HELLO => 36,    // proof(32) + requested_resume_grace_secs(4)
            resume::RESUME => 64, // session_id(16) + proof(32) + offset(8) + offset(8)
            _ => 0,
        };
        let mut rest = vec![0u8; rest_len];
        if rest_len > 0 {
            recv.read_exact(&mut rest)
                .await
                .context("failed to read frame body")?;
        }
        Ok::<_, anyhow::Error>((send, recv, type_byte[0], rest))
    })
    .await
    .context("HELLO timeout")??;

    match frame_type {
        FRAME_HELLO => {
            let mut hello = [0u8; 37];
            hello[0] = FRAME_HELLO;
            hello[1..].copy_from_slice(&rest);
            handle_stream(
                conn,
                send,
                recv,
                hello,
                target,
                session_secret,
                active,
                sessions,
                resume_buffer_size,
                max_resume_grace_secs,
            )
            .await
        }
        resume::RESUME => {
            handle_resume_stream(
                conn,
                send,
                recv,
                &rest,
                target,
                session_secret,
                active,
                sessions,
            )
            .await
        }
        other => {
            let mut send = send;
            reject(&mut send, FRAME_REJECT_UNSUPPORTED).await;
            Err(anyhow!("unexpected frame type: {other:#x}"))
        }
    }
}

/// 拒否フレームを送出し、`finish()` 後に `stopped()` で peer への到達を待ってから返す。
/// これをせずに呼び出し元が即座に `conn` を drop すると、1 byte の応答が飛ぶ前に
/// QUIC connection が暗黙に閉じられ、client 側が payload を読めないことがある
/// （実測で確認済みのバグ）。
async fn reject(send: &mut noq::SendStream, code: u8) {
    if send.write_all(&[code]).await.is_ok() {
        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
    }
}

/// helper 側の HELLO 検証・target 接続・中継を行う。
#[allow(clippy::too_many_arguments)]
async fn handle_stream(
    conn: noq::Connection,
    mut send: noq::SendStream,
    recv: noq::RecvStream,
    hello: [u8; 37],
    target: SocketAddr,
    session_secret: [u8; 32],
    active: Arc<AtomicBool>,
    sessions: SessionTable,
    resume_buffer_size: usize,
    max_resume_grace_secs: u64,
) -> Result<()> {
    if hello[0] != FRAME_HELLO {
        reject(&mut send, FRAME_REJECT_UNSUPPORTED).await;
        return Err(anyhow!("unexpected frame type: {:#x}", hello[0]));
    }
    let client_proof = &hello[1..33];
    let expected = compute_proof(&conn, &session_secret, EXPORTER_LABEL, b"")?;

    if client_proof.ct_eq(expected.as_slice()).unwrap_u8() != 1 {
        reject(&mut send, FRAME_REJECT_AUTH).await;
        return Err(anyhow!("proof mismatch, rejecting"));
    }

    // クライアントが希望する resume-grace 期間（0 = 希望なし）。この値を
    // そのまま信用してsession保持時間(≒リソース消費)を決めるのではなく、
    // このサーバー自身の `--resume-window`（`max_resume_grace_secs`）で
    // clampした上でACKに実効値を返す（ISEKAI_PIPE_DESIGN.md — client任せに
    // しない設計）。
    let requested_resume_grace_secs = u32::from_be_bytes(hello[33..37].try_into().unwrap());
    let effective_resume_grace_secs = effective_resume_grace(requested_resume_grace_secs, max_resume_grace_secs);

    // 同時アクティブ接続は1本まで。target への TCP 接続成功直後に slot を確保する
    // （HELPER_PROTOCOL.md「ハンドシェイクの処理順序」参照）。
    if active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        reject(&mut send, FRAME_REJECT_DUPLICATE).await;
        return Err(anyhow!("duplicate active connection rejected"));
    }

    let mut recv = recv;
    let result = relay_with_resume(
        &conn,
        &mut send,
        &mut recv,
        target,
        session_secret,
        &sessions,
        resume_buffer_size,
        effective_resume_grace_secs,
    )
    .await;
    active.store(false, Ordering::SeqCst);
    result
}

/// Phase 8-3: `RESUME` フレームを検証し、既存セッションに park された TCP
/// 接続を取り戻して中継を再開する。`body` は type byte を除いた 64 byte
/// （session_id(16) + proof(32) + client_sent_offset(8) + client_delivered_offset(8)）。
#[allow(clippy::too_many_arguments)]
async fn handle_resume_stream(
    conn: noq::Connection,
    mut send: noq::SendStream,
    mut recv: noq::RecvStream,
    body: &[u8],
    target: SocketAddr,
    session_secret: [u8; 32],
    active: Arc<AtomicBool>,
    sessions: SessionTable,
) -> Result<()> {
    let mut session_id = [0u8; 16];
    session_id.copy_from_slice(&body[0..16]);
    let client_proof = &body[16..48];
    let client_sent_offset = u64::from_be_bytes(body[48..56].try_into().unwrap());
    let client_delivered_offset = u64::from_be_bytes(body[56..64].try_into().unwrap());

    // resume_proof = HMAC(session_secret, exporter(新connection) || session_id)
    // （HELPER_PROTOCOL.md §7.3。session_id を混ぜることで、同じ session_secret を
    // 使い回す複数セッションが互いの resume トークンを流用できないようにする）。
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .map_err(|e| anyhow!("export_keying_material failed: {e:?}"))?;
    let mut mac = HmacSha256::new_from_slice(&session_secret).expect("HMAC accepts any key length");
    mac.update(&exporter);
    mac.update(&session_id);
    let expected = mac.finalize().into_bytes();
    if client_proof.ct_eq(expected.as_slice()).unwrap_u8() != 1 {
        reject(&mut send, FRAME_REJECT_AUTH).await;
        return Err(anyhow!("resume proof mismatch, rejecting"));
    }

    let Some(handle) = sessions.get(&session_id).await else {
        reject(&mut send, resume::REJECT_UNKNOWN_SESSION).await;
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
        reject(&mut send, resume::REJECT_UNKNOWN_SESSION).await;
        return Err(anyhow!(
            "session {} not resumable (no parked TCP connection)",
            hex_lower(&session_id)
        ));
    };

    if active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        repark(&handle, tcp_read, tcp_write).await;
        reject(&mut send, FRAME_REJECT_DUPLICATE).await;
        return Err(anyhow!("duplicate active connection rejected"));
    }

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
        active.store(false, Ordering::SeqCst);
        repark(&handle, tcp_read, tcp_write).await;
        reject(&mut send, resume::REJECT_OFFSET_GONE).await;
        return Err(anyhow!(
            "requested offset {client_delivered_offset} no longer in output buffer for session {}",
            hex_lower(&session_id)
        ));
    };

    log::info!(
        "resume: session_id={} client_sent_offset={client_sent_offset} client_delivered_offset={client_delivered_offset} \
         helper_committed_offset={helper_committed_offset} replaying {} bytes",
        hex_lower(&session_id),
        replay_bytes.len()
    );

    let mut ack = Vec::with_capacity(17);
    ack.push(resume::RESUME_ACK);
    ack.extend_from_slice(&helper_committed_offset.to_be_bytes());
    ack.extend_from_slice(&helper_sent_offset.to_be_bytes());
    if let Err(e) = send.write_all(&ack).await {
        active.store(false, Ordering::SeqCst);
        repark(&handle, tcp_read, tcp_write).await;
        return Err(anyhow!("failed to write RESUME_ACK: {e}"));
    }
    if !replay_bytes.is_empty() {
        if let Err(e) = send.write_all(&replay_bytes).await {
            active.store(false, Ordering::SeqCst);
            repark(&handle, tcp_read, tcp_write).await;
            return Err(anyhow!("failed to replay buffered output: {e}"));
        }
    }

    // control stream も新しい connection 上で作り直す（元の control stream は
    // 古い connection に紐づいたまま失効している）。8-1 と同じ理由で、
    // 確立を待たずに中継を先に始める。
    let control_task = {
        let conn = conn.clone();
        let handle = handle.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(HELLO_TIMEOUT, accept_control_stream(&conn, session_secret))
                .await
            {
                Ok(Ok((csend, crecv, _new_id))) => {
                    // control stream 上の session_id は毎回新規発行されるが、
                    // resume 後も既存の session_id をそのまま使い続ける
                    // （sessions テーブルのキーは変更しない）。
                    log::info!(
                        "resume: control stream re-established for session_id={}",
                        hex_lower(&session_id)
                    );
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
    active.store(false, Ordering::SeqCst);
    finish_or_park_session(&sessions, Some(session_id), handle, outcome).await;
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
fn compute_proof(
    conn: &noq::Connection,
    session_secret: &[u8; 32],
    label: &[u8],
    context: &[u8],
) -> Result<[u8; 32]> {
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, label, context)
        .map_err(|e| anyhow!("export_keying_material failed: {e:?}"))?;
    let mut mac = HmacSha256::new_from_slice(session_secret).expect("HMAC accepts any key length");
    mac.update(&exporter);
    Ok(mac.finalize().into_bytes().into())
}

/// control stream を accept し、`CONTROL_HELLO` を検証して `session_id` を発行、
/// `CONTROL_ACK` を返す。Phase 8 未対応の client（control stream を開かない）向けに
/// 呼び出し側でタイムアウトを掛けることを想定している。
async fn accept_control_stream(
    conn: &noq::Connection,
    session_secret: [u8; 32],
) -> Result<(noq::SendStream, noq::RecvStream, resume::SessionId)> {
    let (mut csend, mut crecv) = conn.accept_bi().await.context("no control stream opened")?;
    let mut hello = [0u8; 33];
    crecv
        .read_exact(&mut hello)
        .await
        .context("failed to read CONTROL_HELLO")?;
    if hello[0] != resume::CONTROL_HELLO {
        return Err(anyhow!("unexpected control frame type: {:#x}", hello[0]));
    }
    let expected = compute_proof(conn, &session_secret, EXPORTER_LABEL, b"")?;
    if hello[1..33].ct_eq(&expected).unwrap_u8() != 1 {
        return Err(anyhow!("CONTROL_HELLO proof mismatch"));
    }

    let session_id = SessionTable::generate_session_id();
    let mut ack = Vec::with_capacity(17);
    ack.push(resume::CONTROL_ACK);
    ack.extend_from_slice(&session_id);
    csend
        .write_all(&ack)
        .await
        .context("failed to write CONTROL_ACK")?;

    Ok((csend, crecv, session_id))
}

/// target への TCP 接続・ACK 送出の後、中継を**即座に**開始する。
/// output buffer は常に用意するが、control stream の accept を待って中継開始を
/// 遅らせることはしない（旧クライアント・resume 非対応クライアントに対して
/// 無駄な `HELLO_TIMEOUT` 分の遅延を与えないため。当初 control stream の accept
/// を先に `await` してから中継する実装にしていたが、control stream を開かない
/// クライアントとの疎通が最大 `HELLO_TIMEOUT` 秒も遅延するリグレッションを
/// 実際に e2e テストで検出したため、この設計に変更した）。
/// control stream の accept は背後タスクとして並行に進め、確立できたら
/// 実行中の中継が使っている `Session` handle にそのまま APP_ACK タスクを紐付ける。
async fn relay_with_resume(
    conn: &noq::Connection,
    send: &mut noq::SendStream,
    recv: &mut noq::RecvStream,
    target: SocketAddr,
    session_secret: [u8; 32],
    sessions: &SessionTable,
    resume_buffer_size: usize,
    effective_resume_grace_secs: u32,
) -> Result<()> {
    let tcp = match TcpStream::connect(target).await {
        Ok(s) => s,
        Err(e) => {
            reject(send, FRAME_REJECT_TARGET).await;
            return Err(anyhow!("connect to {target} failed: {e}"));
        }
    };
    let mut ack = Vec::with_capacity(5);
    ack.push(FRAME_ACK);
    ack.extend_from_slice(&effective_resume_grace_secs.to_be_bytes());
    send.write_all(&ack).await?;
    let (tcp_read, tcp_write) = tcp.into_split();

    let handle = Arc::new(Mutex::new(Session::new(resume_buffer_size)));
    let established_id: Arc<Mutex<Option<resume::SessionId>>> = Arc::new(Mutex::new(None));

    let control_task = {
        let conn = conn.clone();
        let handle = handle.clone();
        let sessions = sessions.clone();
        let established_id = established_id.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(HELLO_TIMEOUT, accept_control_stream(&conn, session_secret))
                .await
            {
                Ok(Ok((csend, crecv, id))) => {
                    sessions.insert_existing(id, handle.clone()).await;
                    *established_id.lock().await = Some(id);
                    log::info!("control stream established, session_id={}", hex_lower(&id));
                    spawn_app_ack_tasks(csend, crecv, handle);
                }
                Ok(Err(e)) => {
                    log::info!("no resume support for this connection ({e:#})");
                }
                Err(_) => {
                    log::info!("control stream not opened within timeout, continuing without resume support");
                }
            }
        })
    };

    let outcome = relay_buffered(send, recv, tcp_read, tcp_write, handle.clone(), target).await;

    control_task.abort();
    let id = *established_id.lock().await;
    finish_or_park_session(sessions, id, handle, outcome).await;
    Ok(())
}

/// data stream 側の中継ループ終了後、TCP 接続がまだ生きているとみなせるなら
/// `Session::parked_tcp` に戻して resume を待てるようにし、TCP 自体が死んで
/// いるなら（あるいは control stream が一度も確立せず session_id が無いなら）
/// テーブルから破棄する。
async fn finish_or_park_session(
    sessions: &SessionTable,
    id: Option<resume::SessionId>,
    handle: Arc<Mutex<Session>>,
    outcome: RelayOutcome,
) {
    let Some(id) = id else {
        // control stream が確立せず session_id も発行されていない
        // （旧クライアント等）。resume できないので何もしない
        // （handle はテーブルに登録されていないのでこの Arc が drop されて終わり）。
        return;
    };
    match outcome {
        RelayOutcome::TcpDied => {
            log::info!(
                "session {} target connection died, discarding",
                hex_lower(&id)
            );
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
        }
    }
}

/// APP_ACK の送受信を行う背後タスクを spawn する。data stream 側が終わって
/// `relay_with_resume` が呼び出し元の control_task を abort() すれば、
/// control stream 側の read/write もいずれエラーになりループを抜ける。
fn spawn_app_ack_tasks(
    mut csend: noq::SendStream,
    mut crecv: noq::RecvStream,
    session: Arc<Mutex<Session>>,
) {
    // APP_ACK 受信: client からの client_delivered_offset を受け取り、
    // output buffer の破棄範囲を進める。
    {
        let session = session.clone();
        tokio::spawn(async move {
            loop {
                let mut frame = [0u8; 9];
                match crecv.read_exact(&mut frame).await {
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
    send: &mut noq::SendStream,
    recv: &mut noq::RecvStream,
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
            result = recv.read(&mut c2s_buf), if !c2s_done => {
                match result {
                    Ok(Some(n)) => {
                        if let Err(e) = tcp_write.write_all(&c2s_buf[..n]).await {
                            log::warn!("relay to {target}: tcp write failed: {e}");
                            return RelayOutcome::TcpDied;
                        }
                        session.lock().await.helper_committed_offset += n as u64;
                    }
                    Ok(None) => {
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
                        let _ = send.finish();
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
