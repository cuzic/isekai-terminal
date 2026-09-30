# CI / scripts / hooks / iOS コードレビュー指摘タスク(2026-09-29)

- レビュー実施日: 2026-09-29
- 方法: 読み取り専用の静的レビュー(ビルド・テストは実行せず)。iOS(Swift)部分はサブエージェントの
  読み取りレビューを取りまとめ側で再確認したもの。
- 対象: `.github/workflows`, `.github/actions`, `scripts/`, `rust-core/scripts/`, `.claude/hooks`,
  `ios/`(generated除く)
- 起票時点の確認: 全指摘を `origin/main`(0ac06820)のコードで再確認し、すべて今も有効であることを確認した
  (該当ファイルはレビュー以降に変更されていない)。
- 状態の凡例: `[ ]` 未着手 / `[x]` 修正済み / `[-]` 対応不要(理由) / `[~]` 見送り(理由)

## High

- [x] **CI-H1** (High) `.github/workflows/android-test-check.yml:81-82`, `rust-core-test-check.yml:87-88,270-271,333-334`,
  `android-uniffi-drift-check.yml:74-75`, `.github/actions/pr-path-gate/action.yml:86-101,124-127`
  - 要約: pr-path-gate を実行する `changes` job が失敗/キャンセル/タイムアウトすると、`needs: changes` の
    required job(`android-unit-test`/`rust-core-test-linux`/`android-uniffi-drift`)が **skipped** になり、
    branch protection 上は緑扱いで素通りする。
  - 修正方針: required job の `if:` を `!cancelled() && (needs.changes.result != 'success' || needs.changes.outputs.relevant == 'true')`
    にし、gate job が成功しなかった場合は重いテストを実行する(fail-open)。job の `name:`(required の
    context 名)は変更しない。
  - 対応: 3ワークフロー計5 job の `if:` を上記に変更。pr-path-gate の action.yml に呼び出し側の条件付け規約を追記。

## Medium

- [x] **CI-M1** (Medium) `release-build.yml:203`(`softprops/action-gh-release@v2`, `contents: write`)、
  `mlugg/setup-zig@v2`(多数)、`taiki-e/install-action@nextest`(rust-core-test-check.yml:224)、
  `dtolnay/rust-toolchain@stable`(cargo-mutants-check.yml:104)
  - 要約: サードパーティ action がタグ参照のみ。タグ乗っ取りで `releases/latest`(全ホストの自動 bootstrap 配布元)
    に任意バイナリが載りうる。
  - 修正方針: サードパーティ action をすべて commit SHA 固定にし、元のタグをコメントで残す。
  - 対応: `mlugg/setup-zig`(v2.2.1)/`softprops/action-gh-release`(v2)/`taiki-e/install-action`(nextest)/`dtolnay/rust-toolchain`(stable)を
    2026-09-29 時点のタグが指す commit SHA に固定。`actions/*`(GitHub 公式)はタグ参照のまま。
- [x] **CI-M2** (Medium) `release-build.yml:107-109`, `android-uniffi-drift-check.yml:103-106`(required),
  `regenerate-uniffi-bindings.yml:45-48`, `ios-*-check.yml`, `build-android.yml`, `cargo-mutants-check.yml`
  - 要約: `mlugg/setup-zig version: latest` と、バージョン未固定の `cargo install cargo-zigbuild`(release-build.yml は `--locked` もない)。
    `rust-toolchain.toml` もない。
  - 修正方針: zig は他ワークフローと同じ 0.16.0 に固定。cargo-zigbuild は `--locked --version 0.23.4` に固定。
    Rust toolchain の固定(`rust-toolchain.toml`)は担当境界外(`rust-core/` 直下)なので別途判断。
  - 対応: `setup-zig` の `version: latest` を全箇所 0.16.0 に、`cargo install cargo-zigbuild` を全箇所 `--locked --version 0.23.4` に固定。
  - 見送り(部分): `rust-toolchain.toml` による rustc の固定は `[~]`。rust-core ワークスペース全体(他領域のCI・ローカル・
    Android の cargo ビルド)の挙動を変え、MSRV の選定も要るため、担当境界(ci-ios)外の設計判断として残す。
- [x] **CI-M3** (Medium) `ios-rust-core-check.yml`(「Generate Swift bindings and check for drift」)、`ios-logic-linux-check.yml:94-99`
  - 要約: Swift バインディングの drift-check が `git diff --exit-code` のみで、未追跡の新規生成物・新規 `.sha256` を検知できない。
  - 修正方針: Kotlin 側と同様に `git status --porcelain` でも同じパスを確認する。
  - 対応: 両ワークフローの drift-check に `git status --porcelain` の確認を追加し、失敗時に該当ファイル一覧を出すようにした。
- [x] **CI-M4** (Medium) `scripts/device_verify.sh:198,337-339,78-100`
  - 要約: 失敗時にテスト用公開鍵が `~/.ssh/authorized_keys` に残る(EXIT trap で戻さない)。削除時の `grep -v` が
    0行一致で exit 1 して `set -e` で中断する。
  - 修正方針: 削除処理を関数化し、EXIT trap(`cleanup`)から呼ぶ(`--keep` 指定時は残す)。`grep -vF` の終了コード1を許容する。
  - 対応: 削除処理を `remove_test_authorized_key` に関数化し、`cleanup`(EXIT trap)から `--keep` でない限り必ず呼ぶ。
    追記前にフラグを立て、`grep -vF` の終了コード1(残り0行)を許容、一時ファイルは同ディレクトリに作って mv する。
    切り出した関数を一時 HOME で実行し、テスト鍵のみ/他の鍵あり/フラグ未設定の3ケースを確認済み。
- [x] **CI-M5** (Medium) `.claude/hooks/cargo_check_on_edit.py:187-189,198-206`
  - 要約: ワークスペース判定が「crate の親より上で最初に見つかった Cargo.toml」で、`[workspace]` を確認していない。
    独立 workspace の `noq-multipath-spike` 編集で必ず誤報(exit 2)。付随(Low): 診断0件のときキャッシュを更新しないため、
    消えた警告の再発を「新規」と報告しない。
  - 修正方針: crate 自身のディレクトリから上に向かって `[workspace]` を持つ最初の Cargo.toml を選ぶ。
    ビルド成功・診断0件のときは空集合でキャッシュを更新する。Python の単体テストを追加。
  - 対応: `find_workspace_manifest` を追加し、crate 自身のディレクトリから上へ `[workspace]`/`[workspace.*]` を持つ最初の Cargo.toml を選ぶようにした。
    クリーンビルド(診断0件・exit 0)で空集合をキャッシュするようにした。`.claude/hooks/test_cargo_check_on_edit.py`(8件、cargo は実行しないモック)を追加し、ローカルの python3 で全件成功を確認済み。
- [x] **CI-M6** (Medium) `scripts/reserve-room-migration.sh:25-48`, `scripts/reserve-grdb-migration.sh`,
  `scripts/check-room-migrations.sh:67-73`, `scripts/check-grdb-migrations.sh`
  - 要約: 予約スクリプトが自 worktree のレジストリしか見ないため、並列 worktree 同士で同じ番号を取りうる。
    check 側は `[[reserved]]` の重複を検査しない。付随(Low): NEXT > CURRENT+1 のとき、案内が
    `Migration($CURRENT, $NEXT)` という check 違反のコードを表示する。
  - 修正方針: 予約時に `origin/main`(best-effort fetch)と全 worktree のレジストリも見て最大値を取る共通ヘルパーを導入。
    check 側に `[[reserved]]` の重複版数の検出を追加。案内文を `Migration(NEXT-1, NEXT)` に直し、先行予約がある場合の注意を出す。
  - 対応: 共通ヘルパー `scripts/lib/migration-reservation.sh` を追加。自 worktree・`origin/main`(best-effort fetch、`MIGRATION_RESERVE_OFFLINE=1` で省略可)・
    登録済みの全 worktree(未コミットの予約を含む)の current/reserved の最大値+1を予約し、先行予約の出所を表示する。
    案内文は `Migration(NEXT-1, NEXT)` に修正。check-room/grdb に `[[reserved]]` の重複版数の検出を追加。
    一時リポジトリ(bare origin + 別 clone の予約 push + 未コミット予約のある worktree)で、23/24 を避けて 25 を予約することと、重複予約の検出を確認済み。
    既存レジストリに対して両 check スクリプトが OK を返すことも確認済み。
    残る限界: 別マシンで同時に予約して、どちらもまだ push していないケースは原理的に検出できない(main 上の重複検出が最後の砦)。

## Low

- [x] **CI-L1** (Low) `cargo-mutants-check.yml:174-178`, `noq-738-repro-check.yml:74-76`
  - 要約: workflow_dispatch 入力を `run:` に直接展開している(スクリプトインジェクション)。
  - 修正方針: `env:` 経由で渡す(`extra_args` は配列化して分割)。
  - 対応: cargo-mutants は3入力を `env:` 経由にし、`extra_args` は `read -r -a` で配列に分割(再評価なし)。noq-738-repro は `env:` 経由にし、16進 SHA(7-40桁)であることを検証してから Cargo.toml に書く。
- [x] **CI-L2** (Low) `android-test-check.yml:128-131`, `rust-core-test-check.yml`(muslビルド), `ios-logic-linux-check.yml:72-75,81-84`
  - 要約: zig/Swift tarball のチェックサム未検証。
  - 修正方針: zig 0.16.0 tarball の sha256 を固定して検証する。Swift tarball も sha256 を固定して検証する。
  - 対応: zig 0.16.0 tarball(3ワークフロー)と Swift 6.3.3 tarball を、固定した sha256 で `sha256sum -c` 検証してから展開する。
    値は ziglang.org の index.json 公表値と、実際にダウンロードしたバイト列から再計算した値の一致を確認済み(Swift は再計算値)。
    `mlugg/setup-zig` 経由の箇所は action 自身が minisign 署名を検証する。署名(minisign/PGP)そのものの検証は、sha256 固定で十分と判断して行わない。
- [x] **CI-L3** (Low) `release-build.yml:32-36,186-191`
  - 要約: 任意のコミットに `isekai-{ssh,pipe}-v*` タグを打てば `releases/latest` が更新される。
  - 修正方針: publish-release でタグのコミットが `origin/main` の祖先であることを検証してから公開する。
  - 対応: publish-release の先頭で full checkout と `origin/main` の fetch を行い、タグのコミットが `origin/main` の祖先でなければ公開せず失敗させる。
    「required check が緑であること」までは要求しない(main への push では paths フィルタにより一部の required ワークフローがそもそも走らないため、check-run の有無で判定できない)。
- [x] **CI-L4** (Low) `rust-core/scripts/ios-fixture/start-sshd-fixture.sh:21-22`
  - 要約: 相対パスの FIXTURE_DIR で `cd` 後のパスが二重になる。
  - 修正方針: `mkdir -p` 後に絶対パス化する。
  - 対応: `mkdir -p` の直後に `FIXTURE_DIR` を絶対パスへ正規化した。相対パス `relfixture` を渡して、ローカルで起動→fixture.json の絶対パス→停止まで動くことを確認済み。
- [x] **CI-L5** (Low) `android-test-check.yml:70-78`
  - 要約: NDK バージョンの参照元 `fdroid/tools.isekai.terminal.yml` が pr-path-gate のパターンにない。
  - 修正方針: `^fdroid/` を patterns と push.paths に追加。
  - 対応: `^fdroid/` を pr-path-gate の patterns に、`fdroid/**` を push.paths に追加。
- [x] **CI-L6** (Low) `scripts/lib/adb_ui.py:193-196,220-224`
  - 要約: `input text` のエスケープが不完全(`*?~#!{[`、改行、`%`)。
  - 修正方針: デバイス側シェルへは単一引用符でクォートする共通関数に置き換え、表現不能な入力(改行・`%s`)は明示的にエラーにする。単体テストを追加。
  - 対応: `input_text_shell_arg` に集約した。値全体を単一引用符でクォートし、空白は `%s` に変換する。改行とリテラル `%s` は SystemExit で明示的に拒否する。
    `cmd_type`/`cmd_type_terminal` の両方で使う。`scripts/lib/test_adb_ui.py`(7件、ローカル `sh -c` で往復検証)を追加し、全件成功を確認済み。
- [x] **CI-L7** (Low) `rust-core/scripts/android-arm64-clang.sh:4`, `android-arm64-ar.sh:4`, `ndk-common.sh:16`
  - 要約: linker/ar 実行前に cwd を変更している。`$ANDROID_HOME/ndk` が空だと NDK_ROOT が空文字列になる。
  - 修正方針: `cd` をやめて `source "$(dirname …)/ndk-common.sh"`。空の場合は明示的なエラーにする。
  - 対応: ラッパー2本から `cd` を外し、`source "$(dirname "${BASH_SOURCE[0]}")/ndk-common.sh"` に変更。`$ANDROID_HOME/ndk` が空のときは明示的なエラーで終了する。
    偽 NDK で、空ディレクトリのときのエラー(rc=1)と、相対パスで呼んでも呼び出し元の cwd・相対引数のまま exec されることを確認済み。

## Info

- [x] **CI-INFO1** (Info) `scripts/measure_latency.sh:47,16-18`, `scripts/device_verify.sh:30`
  - 要約: `StrictHostKeyChecking=no`、個人用 Tailscale IP とユーザー名が既定値としてハードコードされている。
  - 修正方針: `StrictHostKeyChecking=accept-new` に変更。既定値は環境変数で上書きできるようにする。
  - 対応: `measure_latency.sh` を `StrictHostKeyChecking=accept-new` に変更し、既定ホスト/ユーザーを `ISEKAI_E2E_SSH_HOST`/`ISEKAI_E2E_SSH_USER`(ユーザーの既定は実行ユーザー名)で上書き可能にした。
    `device_verify.sh` の既定ホスト/ポートも `ISEKAI_E2E_SSH_HOST`/`ISEKAI_E2E_SSH_PORT` で上書き可能にした(個人用 IP の既定値自体は後方互換のため残す)。
- [x] **CI-INFO2** (Info) `scripts/device_verify.sh:143`
  - 要約: `./gradlew installDebug` をローカルで実行する(ローカルビルド禁止方針と矛盾)。
  - 修正方針: 既定を「インストール済み前提」に切り替え、ローカルビルドは明示的な `--install-local` 指定時だけにする。GHA でのビルドは android-ci-deploy スキルを案内する。
  - 対応: 既定をインストール済みアプリの使用に変更し、`--apk PATH`(GHA ビルド済み APK を adb install)と `--install-local`(明示時のみ gradle)を追加。`--skip-install` は互換のため受け付ける。
- [~] **CI-INFO3** (Info) `.github/workflows/noq-738-repro-check.yml`
  - 要約: 自称「使い捨て」のワークフローがまだ残っている。
  - 修正方針: noq#738 の再現用として今後も使うかを確認してから削除する。
  - 見送り: noq#738 の修正は cuzic/noq fork で取り込み済みだが、upstream(n0-computer/noq PR #784)はまだレビュー中で、このワークフローは upstream 側の修正 rev を検証する手段として有用。削除は upstream マージ後の判断としてユーザーに委ねる(入力の注入対策は CI-L1 で実施済み)。
- [~] **CI-INFO4** (Info) `.claude/hooks/cargo_check_on_edit.py`(M5の付随)
  - 要約: 編集のたびにローカルで `cargo build` を実行し、「ローカルビルド禁止」の HARD RULE と矛盾している。
  - 修正方針: hook の有効/無効は `.claude/settings.json` 側の運用判断。
  - 見送り: hook を無効化するかどうかは `.claude/settings.json` の運用判断(ユーザーの設定)で、このレビュー修正の範囲では変更しない。M5 の修正で誤報(spike の毎回 exit 2)はなくなった。

## iOS(Swift)

- [ ] **IOS-I1** (Medium) `ios/Sources/IsekaiTerminalCore/TerminalSessionController.swift:174,233,237`
  - 要約: Controller → orchestrator(強参照)→ callback(Controller)の循環参照により、タブを閉じても deinit されない。
    認証情報・scrollback・NWPathMonitor が生き残る。
  - 修正方針: orchestrator には弱参照プロキシ(`WeakOrchestratorCallback`)を渡す。deinit で `orchestrator.disconnect()` と monitor の cancel を行う。
- [ ] **IOS-I2** (Medium) `ios/Sources/IsekaiTerminalCoreLogic/SshHostTrustStore.swift:47,61,73`
  - 要約: `records` に同期がなく、Rust スレッドと main から同時に読み書きされる。`trust` は save 失敗時にメモリだけ更新されたまま残る。
  - 修正方針: `NSLock` で保護する。save 成功後にだけメモリへ反映する。Linux テストを追加。
- [ ] **IOS-I3** (Medium) `SshHostTrustStore.swift:104-106`, `AppServices.swift:25-27`
  - 要約: JSON 破損や identifier の重複で、起動のたびに fatalError(`uniqueKeysWithValues` の trap)。
  - 修正方針: 重複は後勝ちで読み込む。破損時はファイルを退避して空で開く `openRecoveringCorruption` を追加し、AppServices から使う。
- [ ] **IOS-I4** (Medium) `ios/Sources/IsekaiTerminalCore/CredentialVault.swift:80-84`
  - 要約: `rotateKey` が旧 KEK を削除してから store するため、失敗すると秘密鍵を永久に失う。
  - 修正方針: 新 KEK で封緘した blob を一時ファイルへ書く → Keychain を `SecItemUpdate` で差し替える → blob を置き換える、の順にする。
    失敗時は旧 KEK/旧 blob を残し、途中で失敗した場合は Keychain を旧 KEK に戻す。
- [ ] **IOS-I5** (Low-Medium) `CredentialVault.swift:49,158`, `RelayCredentialVault.swift:20,27`
  - 要約: Keychain の読み出しエラー(端末ロック中など)を「鍵なし」とみなして KEK を作り直す。
  - 修正方針: `getOrCreateKey` で新規作成するのは `errSecItemNotFound` のときだけにし、それ以外のエラーは伝播する。
- [ ] **IOS-I6** (Medium) `ios/Sources/IsekaiTerminalCore/TerminalTabsHostView.swift:83-85`
  - 要約: `beginBackgroundTask` の expirationHandler が非同期に `endBackgroundTask` している。
  - 修正方針: handler(main thread で呼ばれる)内で `MainActor.assumeIsolated` を使い、同期的に終了処理を行う。
- [ ] **IOS-I7** (Low-Medium) `TerminalSessionController.swift:181-195,735,948-950,969,982-986,791-795`
  - 要約: 転送状態(`downloadTempURL` 等)がロックなしで複数スレッドから触られる。
  - 修正方針: 転送状態を1つの struct にまとめ、`NSLock` で保護する。
- [ ] **IOS-I8** (Low-Medium) `TerminalSessionController.swift:738`
  - 要約: `FileHandle.readData(ofLength:)` は I/O エラーで ObjC 例外を投げ、Swift では捕捉できない。
  - 修正方針: `read(upToCount:)`(throws)に置き換え、エラー時は転送をキャンセルする。
- [ ] **IOS-L1** (Low・不確実) `TerminalSessionController.swift:798-821,865-889`
  - 要約: コールバックごとに別の `Task { @MainActor }` を作っており、状態更新の順序が保証されない。
  - 修正方針: FIFO が保証される `DispatchQueue.main.async` + `MainActor.assumeIsolated` のヘルパーに統一する。
- [ ] **IOS-L2** (Low) `TerminalSessionController.swift:1012-1033`
  - 要約: agent-sign の保留スロットが1つしかなく、2件目の要求が1件目を上書きする(1件目は30秒ブロックののち拒否)。
  - 修正方針: FIFO キューにし、先頭を表示する。応答・タイムアウトで次の要求を表示する。
- [ ] **IOS-L3** (Low・不確実) `ios/Sources/IsekaiTerminalCore/RemoteClipboardBridge.swift:36-52`
  - 要約: UIPasteboard を Rust スレッドから触っている。
  - 修正方針: write は main へ非同期ディスパッチ、pull は main で同期実行する(呼び出し元が main なら直接実行)。
- [ ] **IOS-L4** (Low) `ios/Sources/IsekaiTerminalCore/TerminalIMEInputView.swift:84,207-211`
  - 要約: composing 中に `insertText` が来ると marked text を確定送信したうえで insertText も送り、二重送信されうる(pinyin 系で要実機確認)。
    `markedTextLog` と `buffer` が際限なく伸びる。
  - 修正方針: 伸長は上限を設けて対処する。二重送信は実機での IME 挙動確認が前提。
- [ ] **IOS-L5** (Low) `ios/Sources/IsekaiTerminalCore/ProfileDatabase.swift:449-451,467`
  - 要約: `jumpKeyEntryId` に FK/ON DELETE SET NULL がなく、鍵を削除すると踏み台プロファイルの参照がぶら下がる。
  - 修正方針: スキーマ変更(migration)ではなく、`deleteKeyEntry` の同一トランザクション内で `jumpKeyEntryId` を NULL に戻す。テストを追加。
