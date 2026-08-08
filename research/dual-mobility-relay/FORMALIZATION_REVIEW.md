# 定式化レビュー: Codex + Fable セカンドオピニオン (2026-07-17)

`dmr/*.py` の数理モデルについて、Codex CLI(`gpt-5.5`, read-only)と Fable エージェント
(独立実行)にそれぞれ同一のブリーフ(`.review_prompt.md`、削除済み)でレビューを依頼した。
両者は完全に独立に実行され、多くの点で収束しつつ、Fableの方がより深く・具体的な指摘を
返した。

## 1. 状態・行動空間

- **両者共通の指摘**: `dmr/warm_standby.py` の `w`(次にinactiveになる方のwarm状態)の
  意味を式で明示すべき(`w_{t+1} = m_t` と書く)。
- **Fable固有・最重要**: この`(c, p, w)`構造は **MOMDP**(mixed-observability MDP;
  Ong, Png, Hsu, Lee 2010)そのもの——隠れ状態は`c`のみ、`(p,w)`は完全観測かつ制御可能。
  名前を与えることで標準的な理論的基盤に接続できる。
- **Fable固有・実は一番重い指摘**: **観測尤度が行動非依存になっているのは物理的に誤り**。
  `simulate_belief_policy_switch`(switching.py)や`policy_eval.py`は、path Aに退避中でも
  standbyがcoldでも、毎ステップ`obs_likelihood[c]`から観測を引いている。現実には
  「B経由のパケット(またはプローブ)が流れているときだけ」hop1/hop2のロスは観測できるはず。
  つまり:
  - Aに退避しstandbyがcoldなら、Bの状態については**観測が得られず**belief は定常分布へ緩和すべき。
  - standbyをwarmに保つことは、**switchコスト低減だけでなく「観測(情報)を買う」行為**でもある
    はず——これはまさにこのプロジェクト全体が問うているVoIの話そのものが、warm/coldの選択に
    まだ組み込まれていないことを意味する。
  - この修正により、STAGE0_REPORTの「Aに一度退避すると戻れない」现象は、現行モデルよりも
    **悪化する**(coldなら本当に何も見えなくなるため)。修正すればwarm standbyの価値も
    正しく評価できるようになる。

## 2. 相関パラメータ rho — **両者とも「要修正」で一致、Fableが具体的な代替を提示**

- **両者共通**: `rho=1`のとき、hop2固有の`p_gb`/`p_bg`が実質使われなくなり、hop2の
  周辺分布(バースト長・定常bad確率)がhop1に引きずられる。つまり**「相関を上げる」つもりで
  「hop2の別の特性(バースト長)まで変えてしまっている」**——Finding 1(「MI gapは相関rhoと
  ともに単調減少」)がこの交絡の影響を受けている可能性がある。
- **Fableの具体的代替案**(closed-form、両ホップの周辺分布を厳密に保持):
  Good<Badの順序でcomonotone coupling(Fréchet upper bound)を使う:
  `P(both Bad) = min(p1', p2')` (2状態行では閉形式)。キーワード:
  *maximal coupling*, *Fréchet–Hoeffding bounds*, *copulas and Markov processes*
  (Darsow–Nguyen–Olsen 1992)。
- **もう一案(より物理的に誠実)**: 隠れた共通環境状態`E`(例:天候/遮蔽)で両ホップの
  パラメータを変調する Markov-modulated Markov chain(状態数は8に増えるが、
  「共有要因」という物理的説明とモデルが一致する)。

## 3. 既存理論との対応 — Fableが具体的な文献・定理を提示

- **Codex**: hysteresis/impulse control、condition-based maintenance POMDPに近い、
  程度の指摘(キーワードのみ)。
- **Fableはここが最も価値が高い**:
  - `mdp.py`(スイッチングコストなし)= **restless bandit のsubsidy問題そのもの**。
    Gilbert-Elliottチャネルに対してWhittle indexが閉形式で解かれている
    (Liu & Zhao 2010, IEEE Trans. IT)。**Finding 2(「switching costなしなら
    myopicなbailが常に最適」)はこの分野の myopic sensing 最適性の結果
    (Ahmad, Liu, Javidi, Zhao, Krishnamachari 2009)の系そのもの**——独自発見というより
    既知の定理の具体例だったと分かる。
  - `switching.py`(スイッチングコスト付き)= 離散版の **optimal switching /
    entry-exit problem**(連続時間版: Ly Vath & Pham 2007)。bandit文脈では
    *bandits with switching costs*(Banks & Sundaram 1994、Asawa & Teneketzis 1996、
    Glazebrook et al.)。**MLR順序 + TP2遷移行列 ⇒ 閾値方策が証明可能**
    (Lovejoy 1987; Krishnamurthy著 “Partially Observed Markov Decision Processes”)。
    GE 2状態チャネルがTP2になる条件は `1 - p_gb ≥ p_bg`(=「持続的なチャネル」、
    現実的なバースト性チャネルなら常に成立)——つまり**閾値構造は数値実験ではなく証明できる**。
  - **具体的で美しい帰結**: rho=0ならbeliefは`(β1,β2)=(P(hop1 bad), P(hop2 bad))`という
    2つの独立スカラーに分解する。最適方策は単位正方形上の**2本の切替曲線**
    (Aへ切替える曲線・Bへ戻る曲線、この2本が一致しないことがまさにhysteresis)で特徴づけられ、
    その形状(β1軸方向 vs β2軸方向の非対称性)こそが Finding 3 の
    「hop2ならbail、hop1なら我慢」という結果の幾何学的な正体。**これをMonte Carloの
    ノイズまみれの数値ではなく、この2曲線として厳密に導出する方が、はるかに鋭い成果物になる**。
  - `warm_standby.py` = 現状は maintenance/standby-with-setup-cost。上記1の観測依存修正後は
    **「観測(センシング)コスト付きrestless bandit」**(Guha & Munagala, FOCS 2007)。
  - **研究としての立ち位置の助言**: 上記はいずれも既知の枠組みなので、独自性を主張すべきは
    制御理論そのものではなく、**「観測チャネルのgarbling(合成 vs 分解)がこのhysteresis
    バンドとそのコストをどう変えるか」という、Blackwell順序をこれらの制御構造の上に重ねる部分**
    ——この組み合わせを扱った先行研究は見当たらないとのこと。論文として書くならこの角度で。

## 4. QMDP近似の妥当性 — Fableがより精密な診断

- **両者共通**: `mdp.py`(行動が遷移に影響しない)ではQMDPは厳密に正しい。
  `switching.py`/`warm_standby.py`(行動が`(active,warm)`遷移に影響する)ではQMDPは近似。
- **Fableのより精密な診断**: 現状のシミュレーションでは観測も行動非依存(上記1参照)なので、
  belief過程は外生的(exogenous)——つまり「情報収集」という概念自体が今のモデルには
  存在せず、QMDPが情報の価値を見誤る古典的な失敗モード(blind to information-gathering)は
  **まだ発現していない**。バイアスは「次ステップ以降は完全観測になる」という将来項の
  楽観視のみ。ただし **上記1の観測修正を入れた瞬間、QMDPは致命的に壊れる**
  ——「warmにしても来ステップ以降どうせ全部見えると仮定する」ため、
  **QMDPはセンシングのためにwarmにする価値をゼロと評価してしまう**。
- **安価な代替案**: belief過程が外生的な現状なら、SARSOPのような重い解法は不要——
  `(belief, p, w)`上のfully-observed MDPとして、belief(rho=0なら2次元)を
  グリッド離散化するだけで厳密解に近づけ、上記3の切替曲線も副産物として得られる。
  観測を行動依存にした後は、この安価な手法も行動依存に拡張すれば引き続き使える
  (真のPOMDPソルバーは必須ではない)。

## 5. その他

- **Fable固有: 評価基準の不整合**。`value_iteration_switch`/`value_iteration_warm`は
  割引(γ=0.95)最適化なのに、`induced_chain_avg_cost`とMonte Carlo評価は**長期平均コスト**
  を見ている——γ-最適方策は平均コスト最適とは限らず、hysteresisバンドの位置はγに依存する。
  平均コスト基準に統一する(relative value iteration)か、一貫して割引評価するべき。
- **Fable固有: 逐次版Blackwell優越も定理として言える**。`voi.py`は一手先のBayesリスクでの
  優越を証明済みだが、**長期(逐次)コストでも「decomposed ≤ composite」は定理として成立する**
  (garblingされた観測しか持たないコントローラーの模倣議論)。つまりMC結果で
  policy_gapが僅かに負になっているセルは**すべて純粋なノイズ**であり、
  「効果の存在」はもはや証明済みで、残る問題は「効果の大きさの推定精度」だけだと言い切れる。
- **Fable固有: c_switchの無次元化提案**。`c_switch / (mean_bad_burst_length × 損失率の差分)`
  という無次元比で再パラメータ化すれば、2次元ヒートマップが1本の曲線に潰れ、
  ピーク位置の解釈・実測値(実際のQUIC path validationコスト)への転用が容易になる
  ——STAGE0_REPORTの「Stage 1では実測値較正を優先すべき」という提言と直結する。
- **両者共通(Codexが先に指摘、Fableも独立に再指摘)**: `voi.py`(観測→決定の一手先)と
  `switching.py`のシミュレーション(予測beliefで決定→観測は次回決定にのみ反映)は
  時系列構造が異なる。「山型」の結果を長期シミュレーションの結果と同一視しない。
- **Fable固有: CUSUM(`changepoint.py`)は現状、意思決定ループから浮いている**。
  `belief_hop_attribution`(厳密フィルタ)がすでに同じ役割を厳密に果たしており、
  CUSUMはその近似的な重複。残すなら quickest-detection理論
  (Page/Lorden/Shiryaev最適性)とswitchingコストを結びつけた形で位置づけるべき。

## 優先順位(両者の指摘を統合した提案)

1. ~~**観測尤度を`(p,w)`依存にする**(§1)~~ — **完了(2026-07-17)**。`switching.py`は
   `action==B`のときのみ、`warm_standby.py`は`action==B`または`(action==A and m==WARM)`
   のときのみ観測可能に修正。結果、switching-cost単体モデルは分解価値が完全にゼロへ潰れ、
   warm standbyモデルでのみ価値が復活することが判明(STAGE0_REPORT.md参照)。
2. ~~**rhoを周辺保持型のcouplingに置き換える**(§2)~~ — **完了(2026-07-17)**。
   `_comonotone_coupling`(Fréchet上限結合)への置き換えで、両ホップの周辺遷移が
   全rhoで厳密に保存されることを数値検証済み。Finding 1の定性的結論は survive。
3. ~~**QMDPの代わりに、beliefグリッドの厳密解を実装する**(§4)~~ — **完了(2026-07-17)**。
   `dmr/beliefgrid.py`(scipy.spatial.Delaunayによる正確な重心座標補間)+
   `dmr/beliefgrid_warm.py`(平均コストRVIでのbelief格子上価値反復)を実装。
   resolution=14での厳密解: gap=0.00092、QMDPのMonte Carlo推定(0.00124±0.00036)と
   同じ桁・同じ符号——QMDPは大きくは間違っていないが、やや過大評価している可能性。
4. ~~**評価基準を平均コストに統一する**(§5)~~ — **完了(2026-07-17)**。
   relative value iteration (RVI) に置き換え、`induced_chain_avg_cost`と厳密一致することを
   検証。副産物として発見: 極端に大きいc_switchでは、一度Aに吸収されると
   「二度と払わない一回限りの費用」は無限期間平均でゼロに埋没するため、gapが山型のまま
   プラトーになる(gain optimality vs bias optimalityの既知の乖離。Puterman ch.8-10)。
   現実的なc_switch範囲(0.05-0.2)には影響しない。
5. ~~**§3の切替曲線(β1,β2の2次元分解)の解析的導出、閾値方策のTP2条件下での証明**~~ —
   **完了(2026-07-18)**、Codex + 独立Fableエージェントによる2度目のレビュー
   (`THRESHOLD_PROOF.md`参照)を経て実施。rho=0では厳密に`(β1,β2)`への分解が
   成り立つことを検証したうえで、`dmr/beliefgrid2d.py`(2次元belief-MDPの厳密解、
   同じ計算量で3-simplexより高解像度)と`dmr/switching_curves.py`(切替曲線抽出)を実装。
   **standbyを常時warmに固定した部分モデルでは、clamp恒等式
   `Δ(β)=clamp(d(β),-c_switch,+c_switch)`によりMLR/TP2による単調方策定理が完全に
   証明できた**(2本の切替曲線は同一のスカラー場`d`の水準集合、hysteresisバンドは
   `{|d(β)|<c_switch}`と厳密に一致)。**一方、完全な4行動モデルでは同じ議論が破綻する
   ことも特定した**——行動が次の`(active_path, warm_status)`文脈自体を選ぶため、
   Q値の比較が異なる観測レジーム間の比較になり、その差(価値の情報量的価値)が
   βに対して山型になり、Topkisの定理が要求する増分の単調性を壊す。数値的には、
   経路(routing)側の閾値構造は試した全シナリオで単調性が崩れなかった(証明はないが
   実験的に頑健)一方、warm/cold側の方策は実際に「閾値ではなく帯(band)」構造を
   具体的なシナリオで発見した——ちょうど経路切替境界の近傍でのみwarmが選ばれる、
   予測されていたVoIの山型構造の直接的な確認。詳細・証明・引用文献は
   `THRESHOLD_PROOF.md`参照。
6. 上記により、Stage 0の成果物は「ノイズの多いMCヒートマップ」から「観測依存化・rho修正・
   平均コスト基準・厳密解ベンチマークで裏取りされ、部分的にはMLR/TP2定理として厳密に
   証明された定量的特性づけ」へ格上げされた。
