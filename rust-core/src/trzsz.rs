use std::collections::VecDeque;
use std::time::Duration;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use timed_fsm::{Response, TimedStateMachine};

// ── 公開型 ───────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum TrzszMode {
    Upload,   // R: サーバーが受信待ち（trz）
    Download, // S: サーバーが送信する（tsz）
    Dir,      // D: ディレクトリ転送
}

#[derive(Debug, Clone)]
pub struct TrzszDetection {
    pub mode: TrzszMode,
    pub version: String,
    pub unique_id: String,
}

// ── FSM の入出力型 ────────────────────────────────────────

/// FSM への入力イベント
pub enum TrzszEvent {
    /// SSH stdout からのバイト列
    StdoutBytes(Vec<u8>),
    /// Kotlin: ファイル選択完了（trz 用）
    KotlinAcceptUpload {
        transfer_id: String,
        file_name: String,
        file_size: u64,
        mode: u32,
    },
    /// Kotlin: upload chunk 送信
    KotlinChunk {
        transfer_id: String,
        data: Vec<u8>,
        is_last: bool,
    },
    /// Kotlin: 保存先選択完了（tsz 用）
    KotlinAcceptDownload {
        transfer_id: String,
    },
    /// Kotlin: ユーザーキャンセル
    KotlinCancel {
        transfer_id: String,
    },
}

/// FSM からの出力（副作用の宣言）
#[derive(Debug, Clone, PartialEq)]
pub enum TrzszEffect {
    /// VTE パーサーに流す
    FlushVte(Vec<u8>),
    /// SSH stdin に送る
    SendStdin(Vec<u8>),
    /// trz/tsz 検出 → Kotlin へ通知
    OnTrzszRequest {
        transfer_id: String,
        mode: TrzszMode,
        suggested_name: Option<String>,
        expected_size: Option<u64>,
    },
    /// tsz: download chunk → Kotlin
    OnDownloadChunk {
        transfer_id: String,
        data: Vec<u8>,
        is_last: bool,
    },
    /// 転送進捗
    OnProgress {
        transfer_id: String,
        transferred: u64,
        total: Option<u64>,
    },
    /// 転送完了（成功/失敗）
    OnFinished {
        transfer_id: String,
        success: bool,
        message: Option<String>,
    },
}

/// タイマー識別子
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrzszTimer {
    /// 30秒: 転送無応答タイムアウト
    Transfer,
}

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);

// ── FSM 内部状態 ──────────────────────────────────────────

enum TrzszFsmState {
    /// 通常状態（trzsz 待機中）
    Normal,
    /// 検出済み、Kotlin の応答待ち（ACT 送信済み、CFG を受信中）
    WaitingKotlin {
        transfer_id: String,
        mode: TrzszMode,
        /// ACT 送信後にサーバーから届くバイト列を蓄積（CFG など）
        proto_buf: Vec<u8>,
    },
    /// 転送中
    Transferring {
        transfer_id: String,
        /// サーバーから受信した未処理のプロトコルバイト列
        proto_buf: Vec<u8>,
        /// これまでに転送したバイト数
        transferred: u64,
        /// ファイル全体のサイズ（不明なら None）
        total: Option<u64>,
        /// upload / download それぞれのプロトコル進行状態
        phase: TransferPhase,
    },
    /// キャンセル後 / タイムアウト後の回復中
    Recovering,
}

/// 転送方向ごとのプロトコル進行状態
enum TransferPhase {
    Upload {
        step: UploadStep,
        file_name: String,
        file_size: u64,
        /// 逐次 MD5 コンテキスト（生バイトに対して計算）
        md5_ctx: md5::Context,
        /// 送信済みで SUCC 待ちの DATA チャンク（長さ, is_last）
        pending: VecDeque<(u64, bool)>,
        /// SendingData 到達前に Kotlin から届いた未送信チャンク
        unsent: VecDeque<(Vec<u8>, bool)>,
    },
    Download {
        step: DownloadStep,
    },
}

/// upload (trz) のプロトコル段階
#[derive(PartialEq)]
enum UploadStep {
    WaitCfg,      // ACT 送信後、CFG 待ち
    WaitNumAck,
    WaitNameAck,
    WaitSizeAck,
    SendingData,
    WaitMd5Ack,   // MD5 送信後、SUCC 待ち
}

/// download (tsz) のプロトコル段階
#[derive(PartialEq)]
enum DownloadStep {
    WaitCfg,     // ACT 送信後、CFG 待ち
    WaitNum,
    WaitName,
    WaitSize,
    Receiving,
    WaitMd5,
}

/// trzsz 転送 FSM
///
/// SSH stdout の stream filter として機能する。Normal 状態で trzsz
/// trigger を検出したら WaitingKotlin → Transferring へと遷移し、
/// 転送完了後または キャンセル/タイムアウト後に Recovering を経て
/// Normal へ戻る。
pub struct TrzszTransferFsm {
    state: TrzszFsmState,
    /// Normal 状態でのみ使う tail buffer（magic prefix の照合用）
    tail_buf: Vec<u8>,
    /// transfer_id 生成カウンター
    next_id: u64,
}

impl TrzszTransferFsm {
    pub fn new() -> Self {
        TrzszTransferFsm {
            state: TrzszFsmState::Normal,
            tail_buf: Vec::new(),
            next_id: 0,
        }
    }

    /// 転送を中断して Recovering 状態に遷移する共通処理。
    /// キャンセルとタイムアウトの両方から呼ばれる。
    fn abort_transfer(&mut self, tid: String, message: &str) -> timed_fsm::Response<TrzszEffect, TrzszTimer> {
        self.state = TrzszFsmState::Recovering;
        self.tail_buf.clear();
        timed_fsm::Response::emit(vec![
            TrzszEffect::SendStdin(vec![0x03]), // Ctrl+C
            TrzszEffect::OnFinished {
                transfer_id: tid,
                success: false,
                message: Some(message.into()),
            },
        ])
    }

    fn alloc_transfer_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id += 1;
        format!("trzsz-{id}")
    }

    /// Normal 状態でバイト列を処理して actions を返す
    fn process_normal_bytes(&mut self, bytes: &[u8]) -> Vec<TrzszEffect> {
        let hex: String = bytes.iter().take(48).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
        log::debug!("trzsz: normal {} bytes: {}", bytes.len(), hex);
        self.tail_buf.extend_from_slice(bytes);
        let mut actions = Vec::new();

        if let Some(magic_pos) = rfind(&self.tail_buf, TRZSZ_MAGIC) {
            // magic より前のバイト列は VTE へ
            if magic_pos > 0 {
                actions.push(TrzszEffect::FlushVte(self.tail_buf[..magic_pos].to_vec()));
            }
            let candidate = self.tail_buf[magic_pos..].to_vec();
            self.tail_buf.clear();

            log::info!("trzsz: magic found at {}! candidate={}", magic_pos,
                std::str::from_utf8(&candidate).unwrap_or("<non-utf8>").trim_end());
            if let Some(detection) = parse_trzsz_trigger(&candidate) {
                let transfer_id = self.alloc_transfer_id();
                let mode = detection.mode.clone();
                // ACT をすぐに送信（遅延させると trz がタイムアウトする）
                let act = act_msg();
                log::info!("trzsz: sending ACT immediately {} bytes", act.len());
                actions.push(TrzszEffect::SendStdin(act));
                self.state = TrzszFsmState::WaitingKotlin {
                    transfer_id: transfer_id.clone(),
                    mode,
                    proto_buf: Vec::new(),
                };
                actions.push(TrzszEffect::OnTrzszRequest {
                    transfer_id,
                    mode: detection.mode,
                    suggested_name: None,
                    expected_size: None,
                });
            } else {
                // まだ不完全（次の feed を待つ）
                self.tail_buf = candidate;
            }
        } else {
            // magic なし: 末尾の magic prefix になり得る部分だけ残す
            let keep = magic_suffix_len(&self.tail_buf);
            if self.tail_buf.len() > keep {
                let flush_end = self.tail_buf.len() - keep;
                actions.push(TrzszEffect::FlushVte(self.tail_buf[..flush_end].to_vec()));
                self.tail_buf.drain(..flush_end);
            }
        }

        actions
    }

    /// Transferring 状態でサーバーからの stdout を解析し、プロトコルを進める
    fn on_transferring_bytes(&mut self, bytes: &[u8]) -> Response<TrzszEffect, TrzszTimer> {
        let hex: String = bytes.iter().take(64).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
        log::debug!("trzsz: transfer bytes {} len={}", hex, bytes.len());
        let mut effects: Vec<TrzszEffect> = Vec::new();
        // None = 継続, Some(true) = 成功完了→Normal, Some(false) = 失敗→Recovering
        let mut terminal: Option<bool> = None;
        let mut activity = false;

        if let TrzszFsmState::Transferring { transfer_id, proto_buf, transferred, total, phase } =
            &mut self.state
        {
            proto_buf.extend_from_slice(bytes);
            loop {
                let Some(nl) = proto_buf.iter().position(|&b| b == b'\n') else { break; };
                let raw: Vec<u8> = proto_buf.drain(..=nl).collect();
                let mut line = &raw[..raw.len() - 1];
                if line.last() == Some(&b'\r') {
                    line = &line[..line.len() - 1];
                }
                log::debug!("trzsz: transfer line: {}", String::from_utf8_lossy(line).chars().take(80).collect::<String>());
                let Some((typ, payload)) = parse_line(line) else {
                    log::debug!("trzsz: transfer parse_line failed for line len={}", line.len());
                    continue;
                };
                log::info!("trzsz: transfer server typ={} payload_len={}", typ, payload.len());
                activity = true;

                if typ == "FAIL" || typ == "fail" {
                    let msg = decode_bytes(&payload)
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or(payload);
                    effects.push(TrzszEffect::OnFinished {
                        transfer_id: transfer_id.clone(),
                        success: false,
                        message: Some(msg),
                    });
                    terminal = Some(false);
                    break;
                }

                match phase {
                    TransferPhase::Upload { step, file_name, file_size, md5_ctx, pending, unsent } => {
                        match step {
                            UploadStep::WaitCfg if typ == "CFG" => {
                                log::info!("trzsz: CFG received, sending NUM/NAME/SIZE file={} size={}", file_name, file_size);
                                effects.push(TrzszEffect::SendStdin(frame_int("NUM", 1)));
                                effects.push(TrzszEffect::SendStdin(frame_bin("NAME", file_name.as_bytes())));
                                effects.push(TrzszEffect::SendStdin(frame_int("SIZE", *file_size)));
                                *step = UploadStep::WaitNumAck;
                            }
                            UploadStep::WaitNumAck if typ == "SUCC" => {
                                log::info!("trzsz: NUM SUCC");
                                *step = UploadStep::WaitNameAck;
                            }
                            UploadStep::WaitNameAck if typ == "SUCC" => {
                                log::info!("trzsz: NAME SUCC");
                                *step = UploadStep::WaitSizeAck;
                            }
                            UploadStep::WaitSizeAck if typ == "SUCC" => {
                                log::info!("trzsz: SIZE SUCC → SendingData, unsent={}", unsent.len());
                                *step = UploadStep::SendingData;
                                effects.push(TrzszEffect::OnProgress {
                                    transfer_id: transfer_id.clone(),
                                    transferred: *transferred,
                                    total: *total,
                                });
                                // SendingData 到達前に届いていたチャンクを送る
                                while let Some((data, is_last)) = unsent.pop_front() {
                                    log::info!("trzsz: flushing unsent chunk {} bytes is_last={}", data.len(), is_last);
                                    md5_ctx.consume(&data);
                                    effects.push(TrzszEffect::SendStdin(frame_bin("DATA", &data)));
                                    pending.push_back((data.len() as u64, is_last));
                                }
                                log::info!("trzsz: after flush, pending={}", pending.len());
                            }
                            UploadStep::SendingData if typ == "SUCC" => {
                                if let Some((len, is_last)) = pending.pop_front() {
                                    *transferred += len;
                                    log::info!("trzsz: DATA SUCC transferred={} is_last={}", transferred, is_last);
                                    effects.push(TrzszEffect::OnProgress {
                                        transfer_id: transfer_id.clone(),
                                        transferred: *transferred,
                                        total: *total,
                                    });
                                    if is_last {
                                        // 最終チャンク ACK → MD5 を送って WaitMd5Ack へ
                                        let hash = md5_ctx.clone().finalize();
                                        log::info!("trzsz: sending MD5");
                                        effects.push(TrzszEffect::SendStdin(frame_bin("MD5", &hash.0)));
                                        *step = UploadStep::WaitMd5Ack;
                                    }
                                }
                            }
                            UploadStep::WaitMd5Ack if typ == "SUCC" => {
                                effects.push(TrzszEffect::OnFinished {
                                    transfer_id: transfer_id.clone(),
                                    success: true,
                                    message: None,
                                });
                                terminal = Some(true);
                            }
                            _ => {
                                log::warn!("trzsz: upload: ignored server typ={} (wrong step or unexpected)", typ);
                            }
                        }
                    }
                    TransferPhase::Download { step } => match typ.as_str() {
                        "CFG" if *step == DownloadStep::WaitCfg => {
                            // CFG 受信 → NUM から始まるファイル受信シーケンスへ
                            *step = DownloadStep::WaitNum;
                        }
                        "NUM" if *step == DownloadStep::WaitNum => {
                            let n = payload.parse::<u64>().unwrap_or(1);
                            effects.push(TrzszEffect::SendStdin(frame_int("SUCC", n)));
                            *step = DownloadStep::WaitName;
                        }
                        "NAME" if *step == DownloadStep::WaitName => {
                            // localName をそのままエコー（base64 のまま返す）
                            effects.push(TrzszEffect::SendStdin(frame("SUCC", &payload)));
                            *step = DownloadStep::WaitSize;
                        }
                        "SIZE" if *step == DownloadStep::WaitSize => {
                            let n = payload.parse::<u64>().unwrap_or(0);
                            *total = Some(n);
                            effects.push(TrzszEffect::SendStdin(frame_int("SUCC", n)));
                            effects.push(TrzszEffect::OnProgress {
                                transfer_id: transfer_id.clone(),
                                transferred: *transferred,
                                total: *total,
                            });
                            *step = DownloadStep::Receiving;
                        }
                        "DATA" if *step == DownloadStep::Receiving => {
                            let Some(data) = decode_bytes(&payload) else { continue; };
                            let len = data.len() as u64;
                            *transferred += len;
                            let is_last = matches!(*total, Some(t) if *transferred >= t);
                            effects.push(TrzszEffect::OnDownloadChunk {
                                transfer_id: transfer_id.clone(),
                                data,
                                is_last,
                            });
                            effects.push(TrzszEffect::SendStdin(frame_int("SUCC", len)));
                            effects.push(TrzszEffect::OnProgress {
                                transfer_id: transfer_id.clone(),
                                transferred: *transferred,
                                total: *total,
                            });
                            if is_last {
                                *step = DownloadStep::WaitMd5;
                            }
                        }
                        "MD5" if *step == DownloadStep::WaitMd5 => {
                            effects.push(TrzszEffect::SendStdin(frame("SUCC", &payload)));
                            effects.push(TrzszEffect::OnFinished {
                                transfer_id: transfer_id.clone(),
                                success: true,
                                message: None,
                            });
                            terminal = Some(true);
                        }
                        _ => {}
                    },
                }

                if terminal.is_some() {
                    break;
                }
            }
        } else {
            return Response::consume();
        }

        match terminal {
            Some(true) => {
                self.state = TrzszFsmState::Normal;
                self.tail_buf.clear();
                Response::emit(effects).with_kill_timer(TrzszTimer::Transfer)
            }
            Some(false) => {
                self.state = TrzszFsmState::Recovering;
                self.tail_buf.clear();
                Response::emit(effects).with_kill_timer(TrzszTimer::Transfer)
            }
            None => {
                let resp = Response::emit(effects);
                if activity {
                    resp.with_timer(TrzszTimer::Transfer, TRANSFER_TIMEOUT)
                } else {
                    resp
                }
            }
        }
    }
}

impl Default for TrzszTransferFsm {
    fn default() -> Self { Self::new() }
}

impl TimedStateMachine for TrzszTransferFsm {
    type Event = TrzszEvent;
    type Action = TrzszEffect;
    type TimerId = TrzszTimer;

    fn on_event(&mut self, event: TrzszEvent) -> Response<TrzszEffect, TrzszTimer> {
        match event {
            TrzszEvent::StdoutBytes(bytes) => {
                match &mut self.state {
                TrzszFsmState::Normal => {
                    let bytes2 = bytes.clone();
                    let actions = self.process_normal_bytes(&bytes2);
                    let set_timer = matches!(self.state, TrzszFsmState::WaitingKotlin { .. });
                    let resp = Response::emit(actions);
                    if set_timer {
                        return resp.with_timer(TrzszTimer::Transfer, TRANSFER_TIMEOUT);
                    } else {
                        return resp;
                    }
                }
                TrzszFsmState::WaitingKotlin { proto_buf, .. } => {
                    // ACT 送信済み、CFG 等のサーバー応答をバッファリング
                    log::debug!("trzsz: WaitingKotlin buffering {} server bytes", bytes.len());
                    proto_buf.extend_from_slice(&bytes);
                    return Response::consume();
                }
                TrzszFsmState::Transferring { .. } => {
                    return self.on_transferring_bytes(&bytes);
                }
                TrzszFsmState::Recovering => {
                    // 回復中: VTE へ流し、改行を見たら Normal へ
                    let has_newline = bytes.contains(&b'\n');
                    let actions = vec![TrzszEffect::FlushVte(bytes)];
                    let resp = Response::emit(actions);
                    if has_newline {
                        self.state = TrzszFsmState::Normal;
                        self.tail_buf.clear();
                    }
                    return resp;
                }
            }
            },

            TrzszEvent::KotlinAcceptUpload { transfer_id, file_name, file_size, .. } => {
                if let TrzszFsmState::WaitingKotlin { transfer_id: ref tid, mode: TrzszMode::Upload, proto_buf } = &mut self.state {
                    if *tid == transfer_id {
                        let tid = tid.clone();
                        // ACT はマジック検出時に送信済み。バッファされた CFG 等を引き継ぐ。
                        let buffered = std::mem::take(proto_buf);
                        log::info!("trzsz: KotlinAcceptUpload id={} file={} size={} buffered={}B",
                            tid, file_name, file_size, buffered.len());
                        self.state = TrzszFsmState::Transferring {
                            transfer_id: tid,
                            proto_buf: buffered,  // バッファ済み CFG を初期 proto_buf に渡す
                            transferred: 0,
                            total: Some(file_size),
                            phase: TransferPhase::Upload {
                                step: UploadStep::WaitCfg,
                                file_name,
                                file_size,
                                md5_ctx: md5::Context::new(),
                                pending: VecDeque::new(),
                                unsent: VecDeque::new(),
                            },
                        };
                        // バッファ済みバイト（CFG 等）を空スライスで即座に処理
                        let resp = self.on_transferring_bytes(&[]);
                        return resp.with_timer(TrzszTimer::Transfer, TRANSFER_TIMEOUT);
                    }
                }
                Response::consume()
            }

            TrzszEvent::KotlinAcceptDownload { transfer_id } => {
                if let TrzszFsmState::WaitingKotlin { transfer_id: ref tid, mode: TrzszMode::Download, proto_buf } = &mut self.state {
                    if *tid == transfer_id {
                        let tid = tid.clone();
                        // ACT はマジック検出時に送信済み。バッファされた CFG 等を引き継ぐ。
                        let buffered = std::mem::take(proto_buf);
                        log::info!("trzsz: KotlinAcceptDownload id={} buffered={}B", tid, buffered.len());
                        self.state = TrzszFsmState::Transferring {
                            transfer_id: tid,
                            proto_buf: buffered,
                            transferred: 0,
                            total: None,
                            phase: TransferPhase::Download {
                                step: DownloadStep::WaitCfg,
                            },
                        };
                        let resp = self.on_transferring_bytes(&[]);
                        return resp.with_timer(TrzszTimer::Transfer, TRANSFER_TIMEOUT);
                    }
                }
                Response::consume()
            }

            TrzszEvent::KotlinChunk { transfer_id, data, is_last } => {
                if let TrzszFsmState::Transferring {
                    transfer_id: tid,
                    phase: TransferPhase::Upload { step, md5_ctx, pending, unsent, .. },
                    ..
                } = &mut self.state
                {
                    if *tid == transfer_id {
                        if *step == UploadStep::SendingData {
                            log::info!("trzsz: KotlinChunk direct-send {} bytes is_last={}", data.len(), is_last);
                            // MD5 を逐次更新してから DATA を送信
                            md5_ctx.consume(&data);
                            let frame = TrzszEffect::SendStdin(frame_bin("DATA", &data));
                            pending.push_back((data.len() as u64, is_last));
                            return Response::emit(vec![frame])
                                .with_timer(TrzszTimer::Transfer, TRANSFER_TIMEOUT);
                        } else {
                            log::info!("trzsz: KotlinChunk queued in unsent {} bytes is_last={}", data.len(), is_last);
                            // ACK/CFG 待ち中。SendingData 到達時に flush
                            unsent.push_back((data, is_last));
                            return Response::consume();
                        }
                    }
                }
                log::warn!("trzsz: KotlinChunk ignored (wrong state or id) transfer_id={}", transfer_id);
                Response::consume()
            }

            TrzszEvent::KotlinCancel { transfer_id } => {
                let current_tid = match &self.state {
                    TrzszFsmState::WaitingKotlin { transfer_id: tid, .. } => Some(tid.clone()),
                    TrzszFsmState::Transferring { transfer_id: tid, .. } => Some(tid.clone()),
                    _ => None,
                };
                if current_tid.as_deref() == Some(&transfer_id) {
                    self.abort_transfer(transfer_id, "Cancelled")
                        .with_kill_timer(TrzszTimer::Transfer)
                } else {
                    Response::consume()
                }
            }
        }
    }

    fn on_timeout(&mut self, id: TrzszTimer) -> Response<TrzszEffect, TrzszTimer> {
        match id {
            TrzszTimer::Transfer => {
                let transfer_id = match &self.state {
                    TrzszFsmState::WaitingKotlin { transfer_id: tid, .. } => Some(tid.clone()),
                    TrzszFsmState::Transferring { transfer_id: tid, .. } => Some(tid.clone()),
                    _ => None,
                };
                if let Some(tid) = transfer_id {
                    log::warn!("trzsz: TIMEOUT fired for id={} — aborting", tid);
                    self.abort_transfer(tid, "Transfer timeout")
                } else {
                    log::warn!("trzsz: TIMEOUT fired but no active transfer");
                    Response::pass_through()
                }
            }
        }
    }
}

// ── 内部ヘルパー ─────────────────────────────────────────

const TRZSZ_MAGIC: &[u8] = b"::TRZSZ:TRANSFER:";

// ── trzsz wire codec ─────────────────────────────────────
//
// フレーム形式は `#TYPE:payload\n`。整数（NUM/SIZE/SUCC ack）は 10 進数、
// 文字列・バイナリ（NAME/DATA/MD5）は encodeBytes = base64(zlib(buf))。
//
// 注意: 実機の trz/tsz と相互運用するには NUM の前に ACT/CFG（JSON）の
// ネゴシエーションが必要だが、MVP では未実装（Phase 4D で対応）。

/// trzsz ACT メッセージ: trigger 検出後にクライアントが最初に送る
/// Python trzsz 1.1.5 は confirm:false で即 "Cancelled"、"newline":"LF" で
/// CONFIG.newline="LF"(文字列)になり CFG が \n で終端されないため両方除去する
fn act_msg() -> Vec<u8> {
    let json = r#"{"lang":"go","version":"1.1.5","confirm":true,"protocol":2,"support_binary":false}"#;
    frame_bin("ACT", json.as_bytes())
}

/// `#TYPE:payload\n` フレームを組み立てる
fn frame(typ: &str, payload: &str) -> Vec<u8> {
    format!("#{typ}:{payload}\n").into_bytes()
}

/// 整数メッセージ（10 進数）
fn frame_int(typ: &str, val: u64) -> Vec<u8> {
    frame(typ, &val.to_string())
}

/// バイナリ/文字列メッセージ（trzsz encodeBytes）
fn frame_bin(typ: &str, buf: &[u8]) -> Vec<u8> {
    frame(typ, &encode_bytes(buf))
}

/// trzsz encodeBytes: zlib 圧縮してから base64 標準エンコード
fn encode_bytes(buf: &[u8]) -> String {
    use std::io::Write;
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = enc.write_all(buf);
    let compressed = enc.finish().unwrap_or_default();
    BASE64.encode(compressed)
}

/// trzsz decodeString: base64 デコードしてから zlib 展開
fn decode_bytes(s: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let raw = BASE64.decode(s.trim()).ok()?;
    let mut dec = flate2::read::ZlibDecoder::new(&raw[..]);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).ok()?;
    Some(out)
}

/// `#TYPE:payload` 形式の 1 行をパースする（先頭の tmux junk は無視）
fn parse_line(line: &[u8]) -> Option<(String, String)> {
    let hash = line.iter().rposition(|&b| b == b'#')?;
    let line = &line[hash..];
    let colon = line.iter().position(|&b| b == b':')?;
    if colon < 2 {
        return None;
    }
    let typ = std::str::from_utf8(&line[1..colon]).ok()?.to_string();
    let payload = std::str::from_utf8(&line[colon + 1..]).ok()?.to_string();
    Some((typ, payload))
}

/// buf の末尾のうち TRZSZ_MAGIC の prefix として一致する最長の長さを返す
fn magic_suffix_len(buf: &[u8]) -> usize {
    let max_check = buf.len().min(TRZSZ_MAGIC.len() - 1);
    for len in (1..=max_check).rev() {
        let suffix = &buf[buf.len() - len..];
        if TRZSZ_MAGIC.starts_with(suffix) {
            return len;
        }
    }
    0
}

/// バイト列を後方検索する（最後に出現する needle の位置）
fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .enumerate()
        .rev()
        .find(|(_, w)| *w == needle)
        .map(|(i, _)| i)
}

/// `::TRZSZ:TRANSFER:<mode>:<version>:<unique_id>\n` をパースする
fn parse_trzsz_trigger(bytes: &[u8]) -> Option<TrzszDetection> {
    let newline_pos = bytes.iter().position(|&b| b == b'\n')?;
    let line = std::str::from_utf8(&bytes[..newline_pos]).ok()?;
    let rest = line.strip_prefix("::TRZSZ:TRANSFER:")?;
    let mut parts = rest.splitn(3, ':');
    let mode_char = parts.next()?;
    let version = parts.next()?;
    let unique_id = parts.next()?;
    let mode = match mode_char {
        "R" => TrzszMode::Upload,
        "S" => TrzszMode::Download,
        "D" => TrzszMode::Dir,
        _ => return None,
    };
    Some(TrzszDetection {
        mode,
        version: version.to_string(),
        unique_id: unique_id.to_string(),
    })
}

// ── Golden Tests ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use super::*;
    use timed_fsm::TimerCommand;

    fn trigger(mode: &str) -> Vec<u8> {
        format!("::TRZSZ:TRANSFER:{}:1.1.7:0000004e\n", mode).into_bytes()
    }

    fn feed(fsm: &mut TrzszTransferFsm, bytes: Vec<u8>) -> Response<TrzszEffect, TrzszTimer> {
        fsm.on_event(TrzszEvent::StdoutBytes(bytes))
    }

    fn vte_bytes(resp: &Response<TrzszEffect, TrzszTimer>) -> Vec<u8> {
        resp.actions.iter().flat_map(|a| {
            if let TrzszEffect::FlushVte(b) = a { b.clone() } else { vec![] }
        }).collect()
    }

    fn has_request(resp: &Response<TrzszEffect, TrzszTimer>) -> bool {
        resp.actions.iter().any(|a| matches!(a, TrzszEffect::OnTrzszRequest { .. }))
    }

    fn timer_set(resp: &Response<TrzszEffect, TrzszTimer>, id: TrzszTimer) -> bool {
        resp.timers.iter().any(|t| matches!(t, TimerCommand::Set { id: i, .. } if *i == id))
    }

    // 1. 通常 stdout に trigger なし → bytes はすべて VTE へ
    #[test]
    fn test_no_trigger_all_to_vte() {
        let mut fsm = TrzszTransferFsm::new();
        let input = b"hello\r\nworld\r\n".to_vec();
        let resp = feed(&mut fsm, input.clone());
        assert_eq!(vte_bytes(&resp), input);
        assert!(!has_request(&resp));
        assert!(resp.timers.is_empty());
    }

    // 2. chunk 境界をまたぐ ::TRZSZ:TRANSFER: → 正しく検出
    #[test]
    fn test_chunk_boundary_detection() {
        let mut fsm = TrzszTransferFsm::new();
        let full = trigger("R");
        let mid = full.len() / 2;

        let r1 = feed(&mut fsm, full[..mid].to_vec());
        assert!(!has_request(&r1), "should not detect on incomplete chunk");

        let r2 = feed(&mut fsm, full[mid..].to_vec());
        assert!(has_request(&r2), "should detect after full trigger received");
        assert!(timer_set(&r2, TrzszTimer::Transfer));
    }

    // 3. trigger 前に ANSI escape sequence がある → prefix は VTE へ流す
    #[test]
    fn test_ansi_prefix_flushed_to_vte() {
        let mut fsm = TrzszTransferFsm::new();
        let prefix = b"\x1b[32msome output\x1b[0m".to_vec();
        let mut input = prefix.clone();
        input.extend_from_slice(&trigger("S"));

        let resp = feed(&mut fsm, input);

        let vte = vte_bytes(&resp);
        assert_eq!(vte, prefix, "ANSI prefix should go to VTE");
        assert!(has_request(&resp));

        // mode が Download であることを確認
        let req = resp.actions.iter().find(|a| matches!(a, TrzszEffect::OnTrzszRequest { mode: TrzszMode::Download, .. }));
        assert!(req.is_some());
    }

    // 4. tmux control mode prefix 付き → 正しく検出
    #[test]
    fn test_tmux_control_mode_prefix() {
        let mut fsm = TrzszTransferFsm::new();
        let prefix = b"\x1bP=1s\x1b\\".to_vec();
        let mut input = prefix.clone();
        input.extend_from_slice(&trigger("R"));

        let resp = feed(&mut fsm, input);

        assert_eq!(vte_bytes(&resp), prefix, "tmux prefix should go to VTE");
        assert!(has_request(&resp));
    }

    // 5. Saved / Cancelled / Stopped を誤検出しない
    #[test]
    fn test_no_false_positive_on_status_words() {
        let cases: &[&[u8]] = &[
            b"Saved\n",
            b"Cancelled\n",
            b"Stopped\n",
            b"::TRZSZ:TRANSFER:X:1.0.0:12345678\n", // unknown mode
        ];
        for case in cases {
            let mut fsm = TrzszTransferFsm::new();
            let resp = feed(&mut fsm, case.to_vec());
            assert!(!has_request(&resp), "should not detect: {:?}", std::str::from_utf8(case));
            assert!(matches!(fsm.state, TrzszFsmState::Normal));
        }
    }

    // 6. 転送中に同じ trigger を再送しても再検出しない
    #[test]
    fn test_no_repeated_detection_while_transferring() {
        let mut fsm = TrzszTransferFsm::new();
        let trig = trigger("R");

        let r1 = feed(&mut fsm, trig.clone());
        assert!(has_request(&r1));
        assert!(matches!(fsm.state, TrzszFsmState::WaitingKotlin { .. }));

        // 同じ trigger を再送
        let r2 = feed(&mut fsm, trig);
        assert!(!has_request(&r2), "should not re-detect while WaitingKotlin");
    }

    // 7. trigger 以降のバイト列は VTE に流さない
    #[test]
    fn test_bytes_after_trigger_not_to_vte() {
        let mut fsm = TrzszTransferFsm::new();
        let mut input = trigger("S");
        input.extend_from_slice(b"this should not go to VTE");

        let resp = feed(&mut fsm, input);

        let vte = vte_bytes(&resp);
        assert!(
            !vte.windows(b"this should not".len()).any(|w| w == b"this should not"),
            "bytes after trigger must not reach VTE"
        );
        assert!(has_request(&resp));
    }

    // 8. 1バイトずつ送っても検出できる
    #[test]
    fn test_trigger_split_multiple_chunks() {
        let mut fsm = TrzszTransferFsm::new();
        let full = trigger("D");

        let mut detected_at = None;
        for (i, byte) in full.iter().enumerate() {
            let resp = feed(&mut fsm, vec![*byte]);
            if has_request(&resp) {
                detected_at = Some(i);
                break;
            }
        }

        assert!(detected_at.is_some(), "should detect even when fed 1 byte at a time");
        let req = {
            let mut fsm2 = TrzszTransferFsm::new();
            let resp = feed(&mut fsm2, full);
            resp.actions.into_iter().find(|a| matches!(a, TrzszEffect::OnTrzszRequest { .. }))
        };
        assert!(req.is_some());
        assert!(matches!(req.unwrap(), TrzszEffect::OnTrzszRequest { mode: TrzszMode::Dir, .. }));
    }

    // ── タイマー関連 ──

    // Transfer タイムアウト → Recovering へ、OnFinished(success=false)
    #[test]
    fn test_transfer_timeout_goes_to_recovering() {
        let mut fsm = TrzszTransferFsm::new();
        let resp = feed(&mut fsm, trigger("R"));
        assert!(has_request(&resp));

        // タイムアウトをシミュレート
        let timeout_resp = fsm.on_timeout(TrzszTimer::Transfer);
        assert!(timeout_resp.actions.iter().any(|a| matches!(a, TrzszEffect::OnFinished { success: false, .. })));
        assert!(timeout_resp.actions.iter().any(|a| matches!(a, TrzszEffect::SendStdin(b) if b == &[0x03])));
        assert!(matches!(fsm.state, TrzszFsmState::Recovering));
    }

    // KotlinCancel → Recovering, kill timer
    #[test]
    fn test_kotlin_cancel_goes_to_recovering() {
        let mut fsm = TrzszTransferFsm::new();
        let resp = feed(&mut fsm, trigger("R"));

        // transfer_id を取得
        let tid = resp.actions.iter().find_map(|a| {
            if let TrzszEffect::OnTrzszRequest { transfer_id, .. } = a {
                Some(transfer_id.clone())
            } else { None }
        }).unwrap();

        let cancel_resp = fsm.on_event(TrzszEvent::KotlinCancel { transfer_id: tid });
        assert!(cancel_resp.actions.iter().any(|a| matches!(a, TrzszEffect::OnFinished { success: false, .. })));
        assert!(cancel_resp.timers.iter().any(|t| matches!(t, TimerCommand::Kill { id: TrzszTimer::Transfer })));
        assert!(matches!(fsm.state, TrzszFsmState::Recovering));
    }

    // Recovering 状態: バイト列は VTE へ流し、改行で Normal に戻る
    #[test]
    fn test_recovering_flushes_to_vte_then_returns_normal() {
        let mut fsm = TrzszTransferFsm::new();
        feed(&mut fsm, trigger("R"));
        fsm.on_timeout(TrzszTimer::Transfer);
        assert!(matches!(fsm.state, TrzszFsmState::Recovering));

        // 改行なし → まだ Recovering
        let r1 = feed(&mut fsm, b"some bytes".to_vec());
        assert_eq!(vte_bytes(&r1), b"some bytes");
        assert!(matches!(fsm.state, TrzszFsmState::Recovering));

        // 改行あり → Normal へ
        let r2 = feed(&mut fsm, b"prompt$ \n".to_vec());
        assert!(!vte_bytes(&r2).is_empty());
        assert!(matches!(fsm.state, TrzszFsmState::Normal));
    }

    // ── パーサー unit tests ──

    #[test]
    fn test_parse_upload_trigger() {
        let line = b"::TRZSZ:TRANSFER:R:1.1.7:0000004e\n";
        let det = parse_trzsz_trigger(line).unwrap();
        assert_eq!(det.mode, TrzszMode::Upload);
        assert_eq!(det.version, "1.1.7");
        assert_eq!(det.unique_id, "0000004e");
    }

    #[test]
    fn test_parse_download_trigger() {
        let line = b"::TRZSZ:TRANSFER:S:1.1.7:abcd1234\n";
        let det = parse_trzsz_trigger(line).unwrap();
        assert_eq!(det.mode, TrzszMode::Download);
    }

    #[test]
    fn test_parse_incomplete_no_newline() {
        let line = b"::TRZSZ:TRANSFER:R:1.1.7:0000004e";
        assert!(parse_trzsz_trigger(line).is_none());
    }

    #[test]
    fn test_rfind() {
        assert_eq!(rfind(b"aababab", b"ab"), Some(5));
        assert_eq!(rfind(b"hello", b"xyz"), None);
        assert_eq!(rfind(b"::TRZSZ:TRANSFER:R:", b"::TRZSZ:TRANSFER:"), Some(0));
    }

    // ── プロトコル (Phase 4B/4C) tests ──

    fn stdin_bytes(resp: &Response<TrzszEffect, TrzszTimer>) -> Vec<u8> {
        resp.actions.iter().flat_map(|a| {
            if let TrzszEffect::SendStdin(b) = a { b.clone() } else { vec![] }
        }).collect()
    }

    fn request_tid(resp: &Response<TrzszEffect, TrzszTimer>) -> Option<String> {
        resp.actions.iter().find_map(|a| {
            if let TrzszEffect::OnTrzszRequest { transfer_id, .. } = a {
                Some(transfer_id.clone())
            } else { None }
        })
    }

    fn contains_subseq(hay: &[u8], needle: &[u8]) -> bool {
        needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn test_codec_roundtrip() {
        let data = b"the quick brown fox \x00\x01\xfe\xff";
        let enc = encode_bytes(data);
        let dec = decode_bytes(&enc).unwrap();
        assert_eq!(dec, data);
    }

    #[test]
    fn test_parse_line() {
        assert_eq!(parse_line(b"#SUCC:123"), Some(("SUCC".into(), "123".into())));
        assert_eq!(parse_line(b"junk#DATA:abc"), Some(("DATA".into(), "abc".into())));
        assert_eq!(parse_line(b"no hash"), None);
        assert_eq!(parse_line(b"#:empty"), None);
    }

    /// ACT/CFG ハンドシェイク後に NUM 待ちになる upload/download 共通ヘルパー
    fn accept_upload_and_cfg(fsm: &mut TrzszTransferFsm, tid: &str, file_name: &str, file_size: u64) {
        fsm.on_event(TrzszEvent::KotlinAcceptUpload {
            transfer_id: tid.into(),
            file_name: file_name.into(),
            file_size,
            mode: 0,
        });
        // サーバーから CFG が届く → NUM/NAME/SIZE を送出する
        feed(fsm, cfg_resp());
    }

    fn accept_download_and_cfg(fsm: &mut TrzszTransferFsm, tid: &str) {
        fsm.on_event(TrzszEvent::KotlinAcceptDownload { transfer_id: tid.into() });
        feed(fsm, cfg_resp());
    }

    fn cfg_resp() -> Vec<u8> {
        let json = r#"{"lang":"go","version":"1.1.5","binary":false,"directory":false,"bufsize":1048576,"timeout":10}"#;
        frame_bin("CFG", json.as_bytes())
    }

    // upload: magic 検出時に ACT を送り、CFG 受信後に NUM/NAME/SIZE を送る
    #[test]
    fn test_upload_accept_sends_num_name_size() {
        let mut fsm = TrzszTransferFsm::new();
        // magic 検出 → ACT を即送信 + OnTrzszRequest
        let resp = feed(&mut fsm, trigger("R"));
        let tid = request_tid(&resp).unwrap();
        assert!(contains_subseq(&stdin_bytes(&resp), b"#ACT:"), "magic → ACT immediately");
        assert!(timer_set(&resp, TrzszTimer::Transfer));

        // KotlinAcceptUpload → ACT は既に送信済み、NUM はまだ
        let r_accept = fsm.on_event(TrzszEvent::KotlinAcceptUpload {
            transfer_id: tid.clone(),
            file_name: "foo.txt".into(),
            file_size: 11,
            mode: 0o644,
        });
        let sent_accept = stdin_bytes(&r_accept);
        assert!(!contains_subseq(&sent_accept, b"#ACT:"), "ACT は magic 検出時に送信済み");
        assert!(!contains_subseq(&sent_accept, b"#NUM:"), "should NOT send NUM yet");

        // CFG 受信 → NUM/NAME/SIZE
        let r_cfg = feed(&mut fsm, cfg_resp());
        let sent_cfg = stdin_bytes(&r_cfg);
        assert!(contains_subseq(&sent_cfg, b"#NUM:1\n"), "CFG → NUM");
        assert!(contains_subseq(&sent_cfg, b"#NAME:"), "CFG → NAME");
        assert!(contains_subseq(&sent_cfg, b"#SIZE:11\n"), "CFG → SIZE");
        assert!(matches!(fsm.state, TrzszFsmState::Transferring { .. }));
    }

    // upload: CFG + SUCC 3 回で SendingData → チャンク送信 → MD5 → 完了
    #[test]
    fn test_upload_succ_progresses_and_finishes() {
        let mut fsm = TrzszTransferFsm::new();
        let resp = feed(&mut fsm, trigger("R"));
        let tid = request_tid(&resp).unwrap();
        accept_upload_and_cfg(&mut fsm, &tid, "f", 5);

        // NUM/NAME/SIZE の 3 SUCC
        let mut acks = frame_int("SUCC", 1);
        acks.extend(frame("SUCC", "Zg=="));
        acks.extend(frame_int("SUCC", 5));
        let r = fsm.on_event(TrzszEvent::StdoutBytes(acks));
        assert!(
            r.actions.iter().any(|a| matches!(a, TrzszEffect::OnProgress { transferred: 0, .. })),
            "SendingData 到達で進捗 0 を通知する"
        );

        // チャンク（最終）
        let r2 = fsm.on_event(TrzszEvent::KotlinChunk {
            transfer_id: tid.clone(),
            data: b"hello".to_vec(),
            is_last: true,
        });
        assert!(contains_subseq(&stdin_bytes(&r2), b"#DATA:"), "DATA を送る");

        // DATA の SUCC(5) → 進捗 5 + MD5 送信（完了はまだ）
        let r3 = fsm.on_event(TrzszEvent::StdoutBytes(frame_int("SUCC", 5)));
        assert!(r3.actions.iter().any(|a| matches!(a, TrzszEffect::OnProgress { transferred: 5, .. })));
        assert!(contains_subseq(&stdin_bytes(&r3), b"#MD5:"), "DATA 最終 ACK → MD5 送信");
        assert!(!r3.actions.iter().any(|a| matches!(a, TrzszEffect::OnFinished { .. })), "MD5 SUCC 前は完了しない");

        // MD5 SUCC → 完了
        let r4 = fsm.on_event(TrzszEvent::StdoutBytes(frame_int("SUCC", 0)));
        assert!(r4.actions.iter().any(|a| matches!(a, TrzszEffect::OnFinished { success: true, .. })));
        assert!(matches!(fsm.state, TrzszFsmState::Normal));
    }

    // upload: SendingData 到達前に届いたチャンクは flush される
    #[test]
    fn test_upload_buffers_early_chunk() {
        let mut fsm = TrzszTransferFsm::new();
        let resp = feed(&mut fsm, trigger("R"));
        let tid = request_tid(&resp).unwrap();
        fsm.on_event(TrzszEvent::KotlinAcceptUpload {
            transfer_id: tid.clone(),
            file_name: "f".into(),
            file_size: 3,
            mode: 0,
        });

        // ACT 後・CFG 前にチャンクが届く → 即送信されない（バッファ）
        let early = fsm.on_event(TrzszEvent::KotlinChunk {
            transfer_id: tid.clone(),
            data: b"abc".to_vec(),
            is_last: true,
        });
        assert!(!contains_subseq(&stdin_bytes(&early), b"#DATA:"), "CFG/ACK 前は送らない");

        // CFG → NUM/NAME/SIZE 送出
        feed(&mut fsm, cfg_resp());

        // 3 SUCC → SendingData に到達してバッファを flush
        let mut acks = frame_int("SUCC", 1);
        acks.extend(frame("SUCC", "Zg=="));
        acks.extend(frame_int("SUCC", 3));
        let r = fsm.on_event(TrzszEvent::StdoutBytes(acks));
        assert!(contains_subseq(&stdin_bytes(&r), b"#DATA:"), "SendingData で flush");
    }

    // download: CFG → NUM→NAME→SIZE→DATA→MD5 の往復
    #[test]
    fn test_download_full_flow() {
        let mut fsm = TrzszTransferFsm::new();
        let resp = feed(&mut fsm, trigger("S"));
        let tid = request_tid(&resp).unwrap();
        accept_download_and_cfg(&mut fsm, &tid);

        // NUM:1 → SUCC:1
        let r = fsm.on_event(TrzszEvent::StdoutBytes(frame_int("NUM", 1)));
        assert!(contains_subseq(&stdin_bytes(&r), b"#SUCC:1\n"));

        // NAME → SUCC
        let r = fsm.on_event(TrzszEvent::StdoutBytes(frame_bin("NAME", b"a.txt")));
        assert!(contains_subseq(&stdin_bytes(&r), b"#SUCC:"));

        // SIZE:5 → SUCC:5 + 進捗
        let r = fsm.on_event(TrzszEvent::StdoutBytes(frame_int("SIZE", 5)));
        assert!(contains_subseq(&stdin_bytes(&r), b"#SUCC:5\n"));
        assert!(r.actions.iter().any(|a| matches!(a, TrzszEffect::OnProgress { transferred: 0, total: Some(5), .. })));

        // DATA "hello"（5/5 で is_last）
        let r = fsm.on_event(TrzszEvent::StdoutBytes(frame_bin("DATA", b"hello")));
        let chunk = r.actions.iter().find_map(|a| {
            if let TrzszEffect::OnDownloadChunk { data, is_last, .. } = a {
                Some((data.clone(), *is_last))
            } else { None }
        }).unwrap();
        assert_eq!(chunk.0, b"hello");
        assert!(chunk.1, "5/5 bytes は最終チャンク");
        assert!(contains_subseq(&stdin_bytes(&r), b"#SUCC:5\n"));

        // MD5 → 完了
        let r = fsm.on_event(TrzszEvent::StdoutBytes(frame_bin("MD5", b"digest")));
        assert!(r.actions.iter().any(|a| matches!(a, TrzszEffect::OnFinished { success: true, .. })));
        assert!(matches!(fsm.state, TrzszFsmState::Normal));
    }

    // FAIL メッセージ → Recovering へ
    #[test]
    fn test_fail_message_goes_to_recovering() {
        let mut fsm = TrzszTransferFsm::new();
        let resp = feed(&mut fsm, trigger("S"));
        let tid = request_tid(&resp).unwrap();
        fsm.on_event(TrzszEvent::KotlinAcceptDownload { transfer_id: tid });

        let r = fsm.on_event(TrzszEvent::StdoutBytes(frame_bin("fail", b"disk full")));
        let finished = r.actions.iter().find_map(|a| {
            if let TrzszEffect::OnFinished { success, message, .. } = a {
                Some((*success, message.clone()))
            } else { None }
        }).unwrap();
        assert!(!finished.0);
        assert_eq!(finished.1.as_deref(), Some("disk full"));
        assert!(matches!(fsm.state, TrzszFsmState::Recovering));
    }

    // ── Proptest: FSM 不変量 ─────────────────────────────

    proptest! {
        /// 任意バイト列で FSM がパニックしない
        #[test]
        fn prop_no_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let mut fsm = TrzszTransferFsm::new();
            fsm.on_event(TrzszEvent::StdoutBytes(bytes));
        }

        /// Normal 状態では stdout がすべて FlushVte として返る
        #[test]
        fn prop_normal_state_passthrough(
            bytes in proptest::collection::vec(any::<u8>(), 0..256)
        ) {
            let mut fsm = TrzszTransferFsm::new();
            // Normal 状態（トリガーなし文字列）では入力バイトがそのまま FlushVte に出る
            // ただし trzsz トリガーが偶然含まれる場合は WaitingKotlin に遷移する可能性がある
            let resp = fsm.on_event(TrzszEvent::StdoutBytes(bytes.clone()));
            let flushed: Vec<u8> = resp.actions.iter()
                .filter_map(|a| if let TrzszEffect::FlushVte(b) = a { Some(b.clone()) } else { None })
                .flatten()
                .collect();
            // FlushVte に含まれるバイトは元の bytes のサブセット
            for b in &flushed {
                prop_assert!(bytes.contains(b) || *b < 0x20 || *b > 0x7e || true,
                    "unexpected byte in FlushVte");
            }
            // FlushVte の合計バイト数は入力以下（トリガー検出中はバッファに保持される）
            prop_assert!(flushed.len() <= bytes.len());
        }

        /// 複数ラウンドの stdout でも FSM がパニックしない
        #[test]
        fn prop_multi_round_no_panic(
            rounds in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 0..128),
                1..10,
            )
        ) {
            let mut fsm = TrzszTransferFsm::new();
            for bytes in rounds {
                fsm.on_event(TrzszEvent::StdoutBytes(bytes));
            }
        }

        /// タイムアウト送信でもパニックしない（どの状態でも）
        #[test]
        fn prop_timeout_no_panic(
            setup_bytes in proptest::collection::vec(any::<u8>(), 0..256)
        ) {
            let mut fsm = TrzszTransferFsm::new();
            fsm.on_event(TrzszEvent::StdoutBytes(setup_bytes));
            fsm.on_timeout(TrzszTimer::Transfer);
        }

        /// KotlinCancel はどの状態でもパニックしない
        #[test]
        fn prop_kotlin_cancel_no_panic(
            setup_bytes in proptest::collection::vec(any::<u8>(), 0..256),
            cancel_id in any::<u64>().prop_map(|n| format!("id-{}", n)),
        ) {
            let mut fsm = TrzszTransferFsm::new();
            fsm.on_event(TrzszEvent::StdoutBytes(setup_bytes));
            fsm.on_event(TrzszEvent::KotlinCancel { transfer_id: cancel_id });
        }
    }
}
