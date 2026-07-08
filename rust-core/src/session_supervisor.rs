use parking_lot::Mutex;

// ── SessionState / ExecutionMode ──────────────────────────

/// セッションの生存状態(接続状態 × バックグラウンド遷移の合成)。PLAN.md「Phase Y」の
/// 外部レビュー(2026-07-04)で提案された8状態FSMをそのまま採用している。
///
/// **注意**: `crate::session_state::SessionState`(1セッション分のVTE/trzszパーサー状態、
/// `pub(crate)`でUniFFI越しには公開されない)とは名前が同じだが別物。こちらは
/// UniFFI越しにKotlin/Swift双方へ公開する「接続ライフサイクル」のFSMで、
/// 実際のターミナル描画状態は一切持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SessionState {
    /// 接続試行前、または完全に切断済み。
    Disconnected,
    /// ハンドシェイク/認証中。
    Connecting,
    /// 接続確立済みでフォアグラウンド相当の通常運用中。
    Active,
    /// バックグラウンド遷移が通知され、`ExecutionMode::Background`の間の猶予
    /// (`prepare_for_background`の`budget_ms`)内で接続維持を試みている状態。
    /// 実際の猶予終了判断はSwift側の`beginBackgroundTask`失効コールバックが正
    /// (Rust/Swiftで基準時計を共有していないため、Rust側でタイマーは持たない。
    /// PLAN.md外部レビュー論点10の`budget_ms`化と同じ理由)。
    Quiescing,
    /// バックグラウンド猶予が尽きた(呼び出し側が`mark_suspended`を呼んだ)、または
    /// OSにプロセスを一時停止/終了された後。実際のトランスポートは既に失われている
    /// 前提で、次にフォアグラウンド復帰した際は再接続(reconnect)が必要になる。
    Suspended,
    /// フォアグラウンド復帰が通知され、再接続/セッション有効性確認を行っている状態。
    Resuming,
    /// 意図的な切断処理が進行中(ユーザーによる切断・アプリ終了通知後の後始末等)。
    Closing,
    /// 完全に終了した終端状態。
    Closed,
}

/// アプリの実行モード。iOSの`UIApplication.didEnterBackground`/
/// `willEnterForeground`、Androidの同等のライフサイクルイベントを集約した結果を表す
/// (PLAN.md外部レビュー論点11の「SceneLifecycleReporter→AppExecutionCoordinator→
/// Rust SessionSupervisor」のうち、Rust側が受け取る最終形)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ExecutionMode {
    Foreground,
    Background,
}

struct SupervisorState {
    session_state: SessionState,
    execution_mode: ExecutionMode,
}

/// `SessionState`×`ExecutionMode`の2軸FSMを保持する、判断ロジックのみのオブジェクト。
/// 意図的にどのtransport(`SshSession`/`IsekaiPipeQuicSession`等)とも結び付けていない
/// (`.claude/rules/rust-ssot.md`が要求する「状態と、それに基づく意思決定ロジックは
/// Rust側に置く」を満たす最小単位として切り出し、実際の接続開始/切断呼び出しは
/// 呼び出し側(Kotlin/Swift)が現在の状態を見て行う。既存`SessionOrchestrator`の
/// `ConnPhase`(Idle/Connecting/Connected)を置き換えるかどうかは別途判断が必要な
/// 大きめの移行のため#24のスコープには含めず、まずはこの新しいFSM自体を
/// 単体テスト可能な形で実装することを優先した。PLAN.md「Phase 1C(#24)実装メモ」参照)。
#[derive(uniffi::Object)]
pub struct SessionSupervisor {
    state: Mutex<SupervisorState>,
}

#[uniffi::export]
pub fn create_session_supervisor() -> std::sync::Arc<SessionSupervisor> {
    std::sync::Arc::new(SessionSupervisor {
        state: Mutex::new(SupervisorState {
            session_state: SessionState::Disconnected,
            execution_mode: ExecutionMode::Foreground,
        }),
    })
}

#[uniffi::export]
impl SessionSupervisor {
    pub fn session_state(&self) -> SessionState {
        self.state.lock().session_state
    }

    pub fn execution_mode(&self) -> ExecutionMode {
        self.state.lock().execution_mode
    }

    /// 接続試行を開始したことを通知する。
    pub fn on_connect_requested(&self) {
        self.state.lock().session_state = SessionState::Connecting;
    }

    /// 接続確立(または再接続成功)を通知する。`Connecting`/`Resuming`のどちらからでも
    /// `Active`へ遷移できる(`Resuming`はフォアグラウンド復帰後の再接続が成功した場合)。
    pub fn on_connected(&self) {
        self.state.lock().session_state = SessionState::Active;
    }

    /// 接続試行が失敗したことを通知する(ハンドシェイク失敗・タイムアウト等)。
    pub fn on_connect_failed(&self) {
        self.state.lock().session_state = SessionState::Disconnected;
    }

    /// 切断(意図的/エラー問わず)を通知する。
    pub fn on_disconnected(&self) {
        self.state.lock().session_state = SessionState::Disconnected;
    }

    /// アプリがバックグラウンドへ遷移したことを通知する。`budget_ms`は
    /// `UIApplication.beginBackgroundTask`等が保証する猶予の目安(実際の期限管理は
    /// 呼び出し側が持つ。PLAN.md外部レビュー論点10参照、Rust側では記録しない)。
    /// `Active`の場合のみ`Quiescing`へ遷移する(`Disconnected`/`Connecting`中に
    /// バックグラウンド化しても新規にセッションを維持し始めるわけではないため、
    /// `session_state`自体は変えない)。
    pub fn prepare_for_background(&self, _budget_ms: u32) {
        let mut s = self.state.lock();
        s.execution_mode = ExecutionMode::Background;
        if s.session_state == SessionState::Active {
            s.session_state = SessionState::Quiescing;
        }
    }

    /// バックグラウンド猶予(`budget_ms`)が尽きた、またはOSにより実際に一時停止/
    /// 終了させられたことを通知する。`Quiescing`中のみ`Suspended`へ遷移する。
    pub fn mark_suspended(&self) {
        let mut s = self.state.lock();
        if s.session_state == SessionState::Quiescing {
            s.session_state = SessionState::Suspended;
        }
    }

    /// アプリがフォアグラウンドへ復帰したことを通知する。`Quiescing`は猶予内に
    /// 復帰できたとみなしそのまま`Active`へ戻す(接続は生きている前提)。
    /// `Suspended`は既に接続が失われている前提のため`Resuming`にし、呼び出し側が
    /// 実際に再接続してから`on_connected()`を呼ぶ必要がある。
    pub fn resume_from_foreground(&self) {
        let mut s = self.state.lock();
        s.execution_mode = ExecutionMode::Foreground;
        s.session_state = match s.session_state {
            SessionState::Quiescing => SessionState::Active,
            SessionState::Suspended => SessionState::Resuming,
            other => other,
        };
    }

    /// メモリ逼迫警告(iOSの`didReceiveMemoryWarning`相当)。OSにプロセスを
    /// 終了される可能性が高まったとみなし、`Quiescing`中であれば猶予を待たず
    /// 保守的に`Suspended`扱いにする(実際に終了されるとは限らないが、次の
    /// フォアグラウンド復帰時に「再接続が必要」側へ倒しておく方が、ユーザーに
    /// 無言で固まった画面を見せるより安全という判断)。
    pub fn memory_warning(&self) {
        let mut s = self.state.lock();
        if s.session_state == SessionState::Quiescing {
            s.session_state = SessionState::Suspended;
        }
    }

    /// アプリ終了(`applicationWillTerminate`相当)。以降の再利用は想定しない終端。
    pub fn application_will_terminate(&self) {
        let mut s = self.state.lock();
        s.session_state = match s.session_state {
            SessionState::Disconnected | SessionState::Closed => SessionState::Closed,
            _ => SessionState::Closing,
        };
    }

    /// `Closing`中の後始末(実トランスポートの切断)が完了したことを通知する。
    pub fn on_terminated(&self) {
        self.state.lock().session_state = SessionState::Closed;
    }
}

// ── Tests ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn new_supervisor() -> std::sync::Arc<SessionSupervisor> {
        create_session_supervisor()
    }

    #[test]
    fn initial_state_is_disconnected_foreground() {
        let sup = new_supervisor();
        assert_eq!(sup.session_state(), SessionState::Disconnected);
        assert_eq!(sup.execution_mode(), ExecutionMode::Foreground);
    }

    #[test]
    fn normal_connect_lifecycle() {
        let sup = new_supervisor();
        sup.on_connect_requested();
        assert_eq!(sup.session_state(), SessionState::Connecting);
        sup.on_connected();
        assert_eq!(sup.session_state(), SessionState::Active);
        sup.on_disconnected();
        assert_eq!(sup.session_state(), SessionState::Disconnected);
    }

    #[test]
    fn connect_failure_returns_to_disconnected() {
        let sup = new_supervisor();
        sup.on_connect_requested();
        sup.on_connect_failed();
        assert_eq!(sup.session_state(), SessionState::Disconnected);
    }

    #[test]
    fn background_while_active_quiesces_then_foreground_within_budget_resumes_active() {
        let sup = new_supervisor();
        sup.on_connect_requested();
        sup.on_connected();

        sup.prepare_for_background(30_000);
        assert_eq!(sup.session_state(), SessionState::Quiescing);
        assert_eq!(sup.execution_mode(), ExecutionMode::Background);

        sup.resume_from_foreground();
        assert_eq!(sup.session_state(), SessionState::Active);
        assert_eq!(sup.execution_mode(), ExecutionMode::Foreground);
    }

    #[test]
    fn background_budget_exhausted_then_foreground_requires_resuming() {
        let sup = new_supervisor();
        sup.on_connect_requested();
        sup.on_connected();
        sup.prepare_for_background(30_000);

        sup.mark_suspended();
        assert_eq!(sup.session_state(), SessionState::Suspended);

        sup.resume_from_foreground();
        assert_eq!(sup.session_state(), SessionState::Resuming);
        assert_eq!(sup.execution_mode(), ExecutionMode::Foreground);

        // 呼び出し側が実際に再接続してon_connected()を呼ぶまではActiveにならない。
        sup.on_connected();
        assert_eq!(sup.session_state(), SessionState::Active);
    }

    #[test]
    fn backgrounding_while_disconnected_does_not_fabricate_a_session() {
        let sup = new_supervisor();
        sup.prepare_for_background(30_000);
        // Active以外でのバックグラウンド化はsession_stateを変えない(そもそも
        // 維持すべきセッションが無いため)。
        assert_eq!(sup.session_state(), SessionState::Disconnected);
        assert_eq!(sup.execution_mode(), ExecutionMode::Background);
    }

    #[test]
    fn backgrounding_while_connecting_does_not_quiesce() {
        let sup = new_supervisor();
        sup.on_connect_requested();
        sup.prepare_for_background(30_000);
        assert_eq!(sup.session_state(), SessionState::Connecting);
        assert_eq!(sup.execution_mode(), ExecutionMode::Background);
    }

    #[test]
    fn memory_warning_while_quiescing_forces_suspended() {
        let sup = new_supervisor();
        sup.on_connect_requested();
        sup.on_connected();
        sup.prepare_for_background(30_000);

        sup.memory_warning();
        assert_eq!(sup.session_state(), SessionState::Suspended);
    }

    #[test]
    fn memory_warning_while_active_is_a_no_op() {
        let sup = new_supervisor();
        sup.on_connect_requested();
        sup.on_connected();

        sup.memory_warning();
        assert_eq!(sup.session_state(), SessionState::Active);
    }

    #[test]
    fn mark_suspended_without_quiescing_is_a_no_op() {
        let sup = new_supervisor();
        sup.on_connect_requested();
        sup.on_connected();

        sup.mark_suspended();
        assert_eq!(sup.session_state(), SessionState::Active);
    }

    #[test]
    fn terminate_while_active_goes_through_closing() {
        let sup = new_supervisor();
        sup.on_connect_requested();
        sup.on_connected();

        sup.application_will_terminate();
        assert_eq!(sup.session_state(), SessionState::Closing);

        sup.on_terminated();
        assert_eq!(sup.session_state(), SessionState::Closed);
    }

    #[test]
    fn terminate_while_already_disconnected_goes_straight_to_closed() {
        let sup = new_supervisor();
        sup.application_will_terminate();
        assert_eq!(sup.session_state(), SessionState::Closed);
    }

    #[test]
    fn resume_from_foreground_while_already_foreground_active_is_stable() {
        let sup = new_supervisor();
        sup.on_connect_requested();
        sup.on_connected();

        sup.resume_from_foreground();
        assert_eq!(sup.session_state(), SessionState::Active);
        assert_eq!(sup.execution_mode(), ExecutionMode::Foreground);
    }
}
