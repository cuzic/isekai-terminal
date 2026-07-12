# isekai-tun 設計書(仮称・検討中)

**ステータス**: 検討中・未着手。`isekai-terminal`本体(SSHクライアント)とは独立した、
別プロジェクト/別バイナリとしての構想。`PLAN.md`のスコープには含まれていない。
本ドキュメントは会話ベースの設計検討をまとめたドラフトであり、実装コミットはまだ無い。

**改訂履歴**: 初稿をCodex CLI(`codex exec -s read-only`)にレビューさせ、
指摘(§10参照)を反映して以下の点を修正済み: relay再利用先を`isekai-transport`から
`isekai-link-masque`(MASQUE/CONNECT-UDPベース)に変更、既存multipathの
物理IF同時保持がdead end扱いである点の明記、Android/iOS/macOSの権限・entitlement
記述の訂正、relay自己再帰の除外routeの追加、セキュリティ/法務項目の追加。

## 1. 背景・動機

Zoom/Google Meetなどの会議アプリは、Wi-Fi⇔セルラーの切り替えや瞬断が起きると
再接続(ICE restart等)が発生し、通話が数秒途切れる。これは`isekai-terminal`が
SSHセッションに対して`isekai-pipe`(QUICマルチパス+roaming+resume)で解決している
問題と本質的に同じクラスの問題である。

Zoom/Meetは非公開プロトコル(一部WebRTCベース)のクローズドなクライアントであり、
プロトコル内部への介入(リバースエンジニアリング)は現実的でもToS的にも避けるべき。
そこで、**アプリケーションプロトコルには一切触れず、OSのネットワーク層(パケットが
どの経路を通ってインターネットに出るか)だけを差し替える**ことで、アプリ側に
一切の変更・協力を要求せずに耐障害性を付与することを目標とする。

## 2. ゴール / 非ゴール

### ゴール

- Zoom/Google Meetの通話中に、ローカルのネットワーク経路(Wi-Fi/セルラー/Ethernet)が
  切り替わっても、会議アプリ自身には見えない形で接続を維持する(あるいは劣化を
  最小化する)。
- 会議アプリ・会議相手・会議プラットフォーム側に一切のインストール・設定変更を
  要求しない(自分のマシン側だけで完結する)。
- Zoom/Meetのプロトコル自体には触れない(リバースエンジニアリング不要)。
- 既存の`rust-core`資産(QUICマルチパス/roaming/relay周りの実装知見)を可能な限り
  再利用する。ただし転用先は`isekai-transport`ではなく`isekai-link-masque`が主候補
  (§4参照。当初案は初稿レビューで見積もりが甘いと指摘され修正した)。

### 非ゴール

- Zoom/Meetの内部プロトコルの解析・模倣(botとして会議に参加する等は対象外。
  これは別の話題であり本設計書では扱わない)。
- 真の意味でのシームレスなマルチパス同時ボンディング(複数回線を同時に使い切る)は
  将来検討。まずは「roaming時に瞬断を減らす」を優先する(`PLAN.md` Phase 7-7/9の
  opportunistic方針を踏襲)。**注**: `rust-core/isekai-transport/src/multipath.rs`は
  remote-address multipath(接続の宛先アドレス切り替え)止まりであり、物理Wi-Fi/
  セルラーの同時保持は`path_health.rs`にある通り既知のdead end(noq #738、
  `PLAN.md` Phase 9-4で対象外と判断済み)。よって「複数物理IFから同一relay宛の
  セッションを維持する」という今回の要件は、既存機構をそのまま転用しても解けない
  ため、best-effortなfailover(切替時に短い再接続を許容)から始める前提とする。
- per-appでの完全な精度(あるアプリの通信だけを寸分違わず捕捉すること)は狙わない。
  宛先IPベースの近似で十分とする(§5参照)。

## 3. 全体アーキテクチャ

```
[ 会議アプリ(Zoom/Meet) ]
        |  (アプリは何も知らない。宛先IPが特定範囲の時だけ以下の経路を通る)
        v
[ ローカルgateway(仮称 isekai-tun) ]
   - TUN/utun/VpnService等でOS標準の仮想インターフェースを作る
   - 宛先IPが会議サービスのCIDR範囲(§5)に一致するトラフィックだけルーティングする
   - 複数の物理インターフェース(Wi-Fi/セルラー/Ethernet)を把握し、
     どれか生きている経路を使ってrelayに到達する
        |  (QUICマルチパス。物理回線の切り替え/瞬断はここで吸収する)
        v
[ 自前のrelay(仮称 isekai-tun serve、SFU近傍リージョンに配置) ]
   - relayは固定の公開アドレスを持ち、ローカルgatewayとの間のセッションを
     roaming/resumeで維持する(isekai-pipeサーバー側と同型)
   - relayから先は素通しではなく、実質的にNATゲートウェイとして動作する:
     UDP/TCPのflow table(5-tuple単位)を保持し、SFU側から見えるsource
     port/addressを1通話中は安定させる。fragment/PMTU/ICMPの扱い、
     flowのidle timeoutも設計対象(§8)
        |
        v
[ Zoom/Meet SFU(相手のインフラ) ]
```

ローカルgatewayとrelayの間だけが自前設計の領域で、relayから先(SFUとの通信)は
自前で解析・改造したプロトコルではなくNAT相当の透過フォワードに徹する。これに
よりZoom/Meet側からは「ユーザーが単にrelay配置先の場所からアクセスしている」
以上の違いが見えない。ただし「透過フォワード」は実装が単純という意味ではなく、
flow状態管理自体は relay 側の主要な実装対象である(§4)。

**自己再帰の防止**: ローカルgatewayの宛先IPルート(§5)には、relay自身のIP
アドレスを明示的に除外(直接の物理経路で到達させる)しておく必要がある。
除外を忘れるとgateway→relay間の接続自体がTUN経由でループしてしまう。

## 4. relay設計(isekai-tun serve)

**再利用先の訂正**: 初稿では`isekai-transport`(SSH向け、`AnyByteStream`/ATTACH/
HELLO/resumeという信頼性のあるstream前提のAPI)を再利用する想定だったが、
会議メディアをQUIC streamに載せるとHOL blockingと再送遅延で逆効果になりうる。
`rust-core/isekai-link-masque`が既に存在し、MASQUE(CONNECT-UDP over HTTP/3)
ベースのunordered/unreliable datagram転送(`h3-datagram`/`qmux`)を実装して
いるため、こちらを再利用ベースの主候補とする。転用できる可能性がある部分:
  - `isekai-link-masque`のdatagram framing、CONNECT-UDP-bind、relay-assigned
    public addressの扱い
  - `rust-core/isekai-pipe/src/engine/resume.rs`の`SessionTable`(sweep/insert/
    fencing slot解放のパターン)は、セッション管理の設計知見としては参照可能
    (そのままの型としての再利用ではない)

作り直しが必要な部分:
  - relay側のflow table実装(§3の自己再帰防止・NAT相当の状態管理)は新規。
    既存crateのどれにも該当機能はない。
  - QoS特性がSSH(信頼性優先・低帯域)と会議メディア(低遅延優先・
    高帯域・パケロス許容)で正反対。QUIC DATAGRAM(非信頼)を基本とし、
    再送よりも「遅延パケットは捨てる」方針に寄せる。
  - MTU/PMTU設計: TUNのIPパケット + トンネルフレーミング + QUICのオーバーヘッドで
    1200〜1280バイト付近の実効MTUに簡単に到達する。会議アプリが送る比較的
    大きめのUDPパケットをどう分割/drop/ICMP "Packet Too Big"応答するかを
    先に決める必要がある(§8)。
  - 帯域が1〜2桁大きい(1通話あたり数Mbps)。relayのスループット・CPU負荷を
    別途検証する必要がある。
- 配置場所: Zoom/MeetのSFUが実際に使うリージョンに近いクラウドVM。
  迂回コストを最小化するため、複数リージョンに置いて動的に選択する余地もある
  (v1では固定1台で十分)。

## 5. 宛先IPベースのsplit routing

per-appフィルタ(WFP callout・NEFilterDataProvider entitlement・Android
`addAllowedApplication`等)は、プラットフォームごとに実装コスト・審査リスクが
大きく異なる(WindowsはWinDivert必須、macOS/iOSはApple審査が絡む)。

代わりに、**宛先IP/CIDRベースのsplit routing**を採用する。これは全プラットフォームの
標準VPN API(TUN/utun/`NEPacketTunnelProvider`の`includedRoutes`/
Android`VpnService`のルート設定)がネイティブにサポートしており、特別な
entitlementやドライバ署名を一切必要としない。

- **Zoom**: 公式に公開されているファイアウォール設定用IPレンジを定期取得し、
  CIDRリストとしてルートに追加する。
- **Google Meet**: GoogleのAnycast/Cloud基盤を他サービスと共有しているため、
  IPレンジ単位では精度が落ちる。当初「ついでに他のGoogle通信もrelay経由に
  なる程度」と見積もっていたが、レビューで指摘の通り実際にはそれだけでは
  済まない可能性がある: 巻き込まれる可能性のあるトラフィック(YouTube視聴・
  Drive同期・Gmail等)がrelayの帯域を圧迫する、企業の情報セキュリティポリシー
  上、業務通信を無関係な個人relayに通すこと自体が問題になりうる、Googleの
  地域判定(コンテンツ availability等)がrelayの所在地に引きずられる、
  などの副作用がある。v1では影響範囲を測定し、許容できない場合はGoogle Meet
  対応を見送る/v2のDNS動的学習を前倒しする判断が必要。
- **陳腐化対策**: 静的CIDRリストは事業者側のレンジ変更で陳腐化するため、
  v2では**DNSレスポンス監視による動的ルート学習**(`*.zoom.us`等の既知
  ドメインの解決結果を監視し、都度ルートを追加する)を検討する。
  v1は静的リストの定期更新で妥協する。

## 6. プラットフォーム別実装方針

| OS | 仮想IF作成 | 権限/審査 | 備考 |
|---|---|---|---|
| Linux | `/dev/net/tun` | 特別な署名不要(CAP_NET_ADMIN) | 最も単純。実装の起点にする |
| Android | `VpnService` | 標準API、Play Store可。**relay向けsocketには`VpnService.protect()`相当の呼び出しが必須**(呼ばないとrelayへのUDP自体がTUN経由でループする=§3の自己再帰と同種の問題) | `isekai-terminal`と同じKotlin/Rust構成が流用しやすい |
| macOS | 素の`utun`(root)または`NEPacketTunnelProvider` | root実行のCLIなら追加entitlement不要。ただし`NEPacketTunnelProvider`をSystem Extensionとして配布する場合は、宛先IP限定(`includedRoutes`)であっても**基本のNetwork Extension entitlement(`com.apple.developer.networking.networkextension`、値`packet-tunnel-provider`)自体は必須**(これは自己申請可能でcontent-filter-provider程の審査リスクはないが、「entitlement不要」ではない) | v1はroot実行のCLIツールとして開始 |
| iOS | `NEPacketTunnelProvider` | 上記macOSと同様、`packet-tunnel-provider` entitlementは必須(MDM不要・自己申請可能だが「entitlement不要」ではない)。`includedRoutes`自体はMDM不要でApp Store配布可 | per-app指定はできないが宛先IPベースなら問題なし |
| Windows | WinTun | アダプタ作成自体は署名済みDLLで完結するが、**route/DNS/firewall例外設定・管理者権限起動・サービス化・インストーラ署名は別途実装対象** | per-app精度は不要になったのでWinDivertは不要。ただし「WinTunのみで完結」ではない |

宛先IPベースに倒したことで、**per-appフィルタ用のWFP自作・WinDivert・
NEFilterDataProvider(content-filter-provider) entitlement申請は不要になった**
(§5の帰結)。ただし各OS標準のVPN API自体が要求する基本entitlement・権限・
周辺実装(route/DNS/firewall/インストーラ)は別途必要であり、「OS標準APIだけで
無審査・無実装コストで完結する」わけではない。

## 7. セキュリティ・法的考慮

- Zoom/Meetのプロトコルを一切解析・改造しないため、リバースエンジニアリング
  禁止条項には抵触しない。
- 個人用VPN/relayとして自分のトラフィックを扱うだけであり、一般的なVPN利用の
  範囲内。
- relayを経由することで通信内容(暗号化済みメディアストリーム)が第三者
  (自分が管理するVM)を通過する点は、relay自体のセキュリティ(自分専用に
  限定するアクセス制御、TLS/QUIC上の認証)を`isekai-pipe`同様に担保する。
- 以下、レビューで追加指摘された論点(未検討):
  - relayのログに何を残すか(宛先IP/ポート/帯域等はメタデータとしても
    個人情報性を持ちうる。会議相手や会議内容の推測材料になりうる)
  - 業務会議の通信を個人管理のrelayに通すこと自体が、勤務先の情報セキュリティ
    ポリシーに抵触する可能性(利用者への注意喚起が必要)
  - 自分以外の第三者に使われる「open relay」化を防ぐアクセス制御(認証・
    許可リスト)
  - 帯域の意図しない大量消費(abuse)への対策
  - relay/gateway間の鍵のローテーション、設定配布(CIDRリスト等)の改竄防止
    (署名付き配布)
  - 将来Android/iOS版をストア配布する場合、VPN機能である旨の開示
    (Play Store/App StoreのVPNアプリ向けポリシー・プライバシーラベル)が必要

## 8. オープンクエスチョン

1. relayのQoS設計(遅延優先のドロップポリシー)を`isekai-link-masque`の
   datagram転送にどう組み込むか。
2. Google MeetのIPレンジの粒度不足をどこまで許容するか(§5)。動的DNS学習を
   v1から入れるべきか。DoH/Happy Eyeballs/IPv6/SVCB・HTTPS RRを使う
   resolverだと単純なDNS監視では捕捉しきれない可能性があり、その場合の
   フォールバック方針も要検討。
3. macOSの`NEPacketTunnelProvider` + System Extension化(entitlement申請込み)
   とroot実行CLIのどちらを正式な実装方針にするか(§6で訂正した通り、
   前者も基本entitlementの自己申請は必要)。
4. relayの帯域・CPU要件の実測(会議メディアはSSHより1〜2桁重い)。
5. `isekai-terminal`本体との関係: 完全に別プロジェクト/別リポジトリにするか、
   `rust-core`内の新規crateとして`isekai-link-masque`を直接参照する形にするか。
6. relay側のflow table・NAT相当ロジック(§3・§4)をゼロから設計する場合の
   実装規模の見積もり。
7. MTU/PMTU(§4)の具体的な処理方針(分割 vs drop vs ICMP応答)。

## 9. 実装ステップ(案)

Codexレビューにより、Zoom/Meet実機での確認より前に「トンネル自体が
packet tunnelとして成立しているか」を検証すべき、との指摘を反映し
順序を変更した。

1. Linux版のみでPoC: `ip netns`不要、宛先IPルートのみのTUN +
   `isekai-link-masque`ベースのrelay一本での**UDP echo/RTP-likeな合成
   トラフィック**の疎通確認(実際のZoom/Meetはまだ使わない)。
2. MTU/IPv6/flowのidle timeout等、§4・§8-7のflow table実装を煮詰める。
3. 実際のZoom/Meetアプリで動作確認し、relayのQoSチューニング(§8-1)を
   実測ベースで調整。
4. Android版(`VpnService`、既存Kotlin/Rust構成の流用。`protect()`呼び出し
   忘れに注意)。
5. macOS版(root実行CLI起点)。
6. Windows/iOS版は上記で設計が固まってから着手。

## 10. レビュー履歴

- 初稿(本ドキュメント作成日)をCodex CLI(`codex exec -s read-only`,
  codex-cli 0.142.5)にレビューさせ、上記の修正を反映済み。レビュー時点の
  主な指摘は本ドキュメントの各所に注記として残している(改訂履歴参照)。
