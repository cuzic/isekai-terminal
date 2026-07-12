//! #22: [`crate::rebind_manager::RebindManager`](純粋状態機械)と
//! [`crate::rebind_ports`]のI/Oポート(trait)を配線する非同期実行層。
//! `session.rs`の`TokioTimerRuntime`/`SideEffect`ディスパッチと同じパターンを
//! `RebindTimer`/`RebindAction`向けに踏襲している。実`tokio`/実fd/実I/Oに
//! 触れるのはこのモジュールだけに限定し、判断ロジック自体は一切持たない
//! (すべて`RebindManager`に委譲する)。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use timed_fsm::{Response, TimedStateMachine, TimerCommand};

use crate::rebind_manager::{RebindAction, RebindEvent, RebindManager, RebindPublicState, RebindTimer};
use crate::rebind_ports::{BoundFd, PlatformFdSource, RebindExecutor, WifiProbeExecutor};

/// #19: `RebindManager`の状態変化をUI(Kotlin/Swift)へ伝えるcallback。
/// `RebindAction::PublishState`をDriverが受け取るたびに呼ばれる。
pub(crate) trait RebindStateObserver: Send + Sync {
    fn on_state_changed(&self, state: RebindPublicState);
}

/// `session.rs`の`TokioTimerRuntime`と同じパターン: `RebindTimer`の
/// `TimerCommand::Set`/`Kill`をtokioタスクのspawn/abortへ変換し、満了したら
/// `timeout_tx`経由でイベントループへ送り返す。
struct RebindTimerRuntime {
    handles: HashMap<RebindTimer, tokio::task::JoinHandle<()>>,
    timeout_tx: tokio::sync::mpsc::Sender<RebindTimer>,
}

impl RebindTimerRuntime {
    fn new(timeout_tx: tokio::sync::mpsc::Sender<RebindTimer>) -> Self {
        RebindTimerRuntime { handles: HashMap::new(), timeout_tx }
    }

    fn set(&mut self, id: RebindTimer, dur: Duration) {
        self.kill(id);
        let tx = self.timeout_tx.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(dur).await;
            let _ = tx.send(id).await;
        });
        self.handles.insert(id, handle);
    }

    fn kill(&mut self, id: RebindTimer) {
        if let Some(h) = self.handles.remove(&id) {
            h.abort();
        }
    }

    fn apply(&mut self, cmds: &[TimerCommand<RebindTimer>]) {
        for cmd in cmds {
            match *cmd {
                TimerCommand::Set { id, duration } => self.set(id, duration),
                TimerCommand::Kill { id } => self.kill(id),
            }
        }
    }
}

enum FdKind {
    Wifi,
    Cellular,
}

/// 呼び出し元(`orchestrator.rs`/`multipath_transport.rs`)がDriverへイベントを
/// 送るためのハンドル。実行ループ自体は`spawn_rebind_driver`が所有するtokio
/// タスクの中にある。
#[derive(Clone)]
pub(crate) struct RebindDriverHandle {
    input_tx: tokio::sync::mpsc::Sender<RebindEvent>,
}

impl RebindDriverHandle {
    /// キューが詰まっている/ループが既に終了している場合は黙って捨てる
    /// (既存の`rebind_to_fd`の`try_send`失敗時と同じ日和見的ポリシー)。
    pub(crate) fn send_event(&self, event: RebindEvent) {
        let _ = self.input_tx.try_send(event);
    }
}

/// `RebindManager`の実行ループを`tokio::spawn`し、外部からイベントを送れる
/// ハンドルを返す。`F`/`W`/`R`は#10で定義したI/Oポート(trait)の実装。
pub(crate) fn spawn_rebind_driver<F, W, R>(
    fd_source: Arc<F>,
    probe: Arc<W>,
    executor: Arc<R>,
    observer: Arc<dyn RebindStateObserver>,
) -> RebindDriverHandle
where
    F: PlatformFdSource + 'static,
    W: WifiProbeExecutor + 'static,
    R: RebindExecutor + 'static,
{
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<RebindEvent>(16);
    let (timeout_tx, mut timeout_rx) = tokio::sync::mpsc::channel::<RebindTimer>(8);
    let loop_input_tx = input_tx.clone();

    tokio::spawn(async move {
        let mut manager = RebindManager::new();
        let mut timers = RebindTimerRuntime::new(timeout_tx);
        loop {
            let resp = tokio::select! {
                maybe_event = input_rx.recv() => {
                    let Some(event) = maybe_event else { break };
                    manager.on_event(event)
                }
                maybe_timer = timeout_rx.recv() => {
                    let Some(timer_id) = maybe_timer else { break };
                    manager.on_timeout(timer_id)
                }
            };
            dispatch(&mut timers, resp, &fd_source, &probe, &executor, &observer, &loop_input_tx);
        }
    });

    RebindDriverHandle { input_tx }
}

/// `Response`のtimer commandsを`timers`へ即座に反映し、actionsはそれぞれ
/// 個別のtokioタスクとして実行する(疎通確認/rebindの完了を待つ間、次の
/// イベントの取りこぼしを防ぐため、`dispatch`自体はブロックしない)。
fn dispatch<F, W, R>(
    timers: &mut RebindTimerRuntime,
    resp: Response<RebindAction, RebindTimer>,
    fd_source: &Arc<F>,
    probe: &Arc<W>,
    executor: &Arc<R>,
    observer: &Arc<dyn RebindStateObserver>,
    input_tx: &tokio::sync::mpsc::Sender<RebindEvent>,
) where
    F: PlatformFdSource + 'static,
    W: WifiProbeExecutor + 'static,
    R: RebindExecutor + 'static,
{
    timers.apply(&resp.timers);
    for action in resp.actions {
        match action {
            RebindAction::PublishState(state) => observer.on_state_changed(state),
            RebindAction::PerformRebindToCellular => {
                spawn_acquire_and_rebind(fd_source.clone(), executor.clone(), FdKind::Cellular);
            }
            RebindAction::PerformRebindToWifi => {
                spawn_acquire_and_rebind(fd_source.clone(), executor.clone(), FdKind::Wifi);
            }
            RebindAction::StartWifiProbe => {
                spawn_probe(fd_source.clone(), probe.clone(), input_tx.clone());
            }
        }
    }
}

fn spawn_acquire_and_rebind<F, R>(fd_source: Arc<F>, executor: Arc<R>, kind: FdKind)
where
    F: PlatformFdSource + 'static,
    R: RebindExecutor + 'static,
{
    tokio::spawn(async move {
        let fd = match kind {
            FdKind::Wifi => fd_source.acquire_wifi_fd().await,
            FdKind::Cellular => fd_source.acquire_cellular_fd().await,
        };
        match fd {
            Some(fd) => executor.rebind(fd),
            None => log::warn!("rebind_driver: rebind requested but PlatformFdSource returned no fd"),
        }
    });
}

fn spawn_probe<F, W>(fd_source: Arc<F>, probe: Arc<W>, input_tx: tokio::sync::mpsc::Sender<RebindEvent>)
where
    F: PlatformFdSource + 'static,
    W: WifiProbeExecutor + 'static,
{
    tokio::spawn(async move {
        let Some(fd): Option<BoundFd> = fd_source.acquire_wifi_fd().await else {
            // WiFi自体が使えない(fdが取れない)場合は疎通確認失敗として扱う。
            let _ = input_tx.try_send(RebindEvent::WifiProbeFailed);
            return;
        };
        let ok = probe.probe(fd).await;
        let event = if ok { RebindEvent::WifiProbeSucceeded } else { RebindEvent::WifiProbeFailed };
        let _ = input_tx.try_send(event);
    });
}

// ── テスト ───────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Notify;

    #[derive(Default)]
    struct FakeFdSource {
        wifi_calls: AtomicUsize,
        cellular_calls: AtomicUsize,
        wifi_available: std::sync::atomic::AtomicBool,
    }

    impl FakeFdSource {
        fn new(wifi_available: bool) -> Self {
            FakeFdSource {
                wifi_calls: AtomicUsize::new(0),
                cellular_calls: AtomicUsize::new(0),
                wifi_available: std::sync::atomic::AtomicBool::new(wifi_available),
            }
        }
    }

    impl PlatformFdSource for FakeFdSource {
        fn acquire_wifi_fd(&self) -> impl std::future::Future<Output = Option<BoundFd>> + Send {
            self.wifi_calls.fetch_add(1, Ordering::SeqCst);
            let available = self.wifi_available.load(Ordering::SeqCst);
            async move {
                available.then(|| BoundFd { fd: 42, local_ip: "192.168.0.2".parse().unwrap() })
            }
        }
        fn acquire_cellular_fd(&self) -> impl std::future::Future<Output = Option<BoundFd>> + Send {
            self.cellular_calls.fetch_add(1, Ordering::SeqCst);
            async move { Some(BoundFd { fd: 43, local_ip: "10.0.0.2".parse().unwrap() }) }
        }
    }

    struct FakeProbe {
        succeeds: std::sync::atomic::AtomicBool,
        calls: AtomicUsize,
    }

    impl FakeProbe {
        fn new(succeeds: bool) -> Self {
            FakeProbe { succeeds: std::sync::atomic::AtomicBool::new(succeeds), calls: AtomicUsize::new(0) }
        }
    }

    impl WifiProbeExecutor for FakeProbe {
        fn probe(&self, _fd: BoundFd) -> impl std::future::Future<Output = bool> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let ok = self.succeeds.load(Ordering::SeqCst);
            async move { ok }
        }
    }

    #[derive(Default)]
    struct FakeExecutor {
        rebinds: StdMutex<Vec<(i32, String)>>,
        notify: Notify,
    }

    impl RebindExecutor for FakeExecutor {
        fn rebind(&self, fd: BoundFd) {
            self.rebinds.lock().unwrap().push((fd.fd, fd.local_ip.to_string()));
            self.notify.notify_one();
        }
    }

    #[derive(Default)]
    struct RecordingObserver {
        states: StdMutex<Vec<RebindPublicState>>,
    }

    impl RebindStateObserver for RecordingObserver {
        fn on_state_changed(&self, state: RebindPublicState) {
            self.states.lock().unwrap().push(state);
        }
    }

    /// `cond()`が真になるまで短い間隔でポーリングする(実I/O(fake含む)は
    /// 一瞬で終わるはずなので、上限は寛容だが待ち時間自体は短く保つ)。
    async fn wait_until(mut cond: impl FnMut() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not met within timeout");
    }

    #[tokio::test]
    async fn no_viable_path_triggers_cellular_rebind_and_publishes_state() {
        let fd_source = Arc::new(FakeFdSource::new(true));
        let probe = Arc::new(FakeProbe::new(true));
        let executor = Arc::new(FakeExecutor::default());
        let observer = Arc::new(RecordingObserver::default());

        let handle = spawn_rebind_driver(fd_source.clone(), probe.clone(), executor.clone(), observer.clone());
        handle.send_event(RebindEvent::NoViablePath);

        wait_until(|| !executor.rebinds.lock().unwrap().is_empty()).await;
        assert_eq!(executor.rebinds.lock().unwrap()[0], (43, "10.0.0.2".to_string()));
        assert!(observer.states.lock().unwrap().contains(&RebindPublicState::FailedOverToCellular));
    }

    #[tokio::test]
    async fn manual_force_return_probe_success_rebinds_to_wifi() {
        let fd_source = Arc::new(FakeFdSource::new(true));
        let probe = Arc::new(FakeProbe::new(true));
        let executor = Arc::new(FakeExecutor::default());
        let observer = Arc::new(RecordingObserver::default());

        let handle = spawn_rebind_driver(fd_source.clone(), probe.clone(), executor.clone(), observer.clone());
        handle.send_event(RebindEvent::NoViablePath);
        wait_until(|| !executor.rebinds.lock().unwrap().is_empty()).await;
        executor.rebinds.lock().unwrap().clear();

        handle.send_event(RebindEvent::ManualForceReturnRequested);

        wait_until(|| !executor.rebinds.lock().unwrap().is_empty()).await;
        assert_eq!(executor.rebinds.lock().unwrap()[0], (42, "192.168.0.2".to_string()));
        assert!(observer.states.lock().unwrap().contains(&RebindPublicState::OnWifi));
    }

    #[tokio::test]
    async fn wifi_unavailable_during_probe_is_reported_as_probe_failure() {
        // WiFi自体のfdが取れない(WiFi圏外等) → StartWifiProbeはWifiProbeFailedに
        // 変換されるべきで、Driverがpanicしたり黙って詰まったりしないことを確認する。
        let fd_source = Arc::new(FakeFdSource::new(false));
        let probe = Arc::new(FakeProbe::new(true));
        let executor = Arc::new(FakeExecutor::default());
        let observer = Arc::new(RecordingObserver::default());

        let handle = spawn_rebind_driver(fd_source.clone(), probe.clone(), executor.clone(), observer.clone());
        handle.send_event(RebindEvent::NoViablePath);

        // セルラーへのフェイルオーバーはfd取得に成功するので実行される。
        wait_until(|| !executor.rebinds.lock().unwrap().is_empty()).await;
        // probeはfd自体が取れないので一度も呼ばれない。
        wait_until(|| fd_source.wifi_calls.load(Ordering::SeqCst) > 0).await;
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cellular_fd_unavailable_logs_and_does_not_panic() {
        // FakeFdSourceを部分的に上書きできないので、専用のfakeをここだけ用意する。
        struct NoCellularFdSource;
        impl PlatformFdSource for NoCellularFdSource {
            fn acquire_wifi_fd(&self) -> impl std::future::Future<Output = Option<BoundFd>> + Send {
                async { None }
            }
            fn acquire_cellular_fd(&self) -> impl std::future::Future<Output = Option<BoundFd>> + Send {
                async { None }
            }
        }
        let fd_source = Arc::new(NoCellularFdSource);
        let probe = Arc::new(FakeProbe::new(true));
        let executor = Arc::new(FakeExecutor::default());
        let observer = Arc::new(RecordingObserver::default());

        let handle = spawn_rebind_driver(fd_source, probe, executor.clone(), observer.clone());
        handle.send_event(RebindEvent::NoViablePath);

        // PublishStateだけは同期的に発火するはずなので、それが届くまで待てば
        // 非同期側のfd取得(→None→rebind呼ばれず)も追い付いている。
        wait_until(|| observer.states.lock().unwrap().contains(&RebindPublicState::FailedOverToCellular)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(executor.rebinds.lock().unwrap().is_empty(), "fdが取れないrebindは実行されないはず");
    }
}
