# 論文構成案: Warm/Cold Standby境界の解析とBerlin V2X実データ検証

作成: 2026-07-19。`THRESHOLD_PROOF.md` §9.1/9.2の文献調査(2026-07-19)を踏まえた、
査読耐性を意識した構成案。**理論部分の新規性は限定的**(先行研究と閾値条件・技法が
重なる部分がある)と判明したため、「画期的発見」ではなく「実データで検証された、
きちんと基礎づけられた設計判断」を主軸に据える構成にしている。

## 位置づけの決定

- **想定ジャンル**: 理論一発モノの決定理論/OR論文ではなく、**実測駆動の応用論文**
  (measurement + modeling)。強い部分(Berlin V2X実データでの検証、頑健性解析)を
  主役にし、理論(閉形式・定理)は「なぜそうなるかを説明する道具」として脇に置く。
- **想定投稿先**: ネットワーキング/システム系ジャーナル・ワークショップ
  (measurement-and-modeling寄りの track)、または応用色の強いOR/決定理論誌の
  short paper。理論そのものの新規性を売りにする決定理論の flagship 誌は狙わない
  (§9.1/9.2の文献調査で、核心的な閾値条件・証明技法がいずれも先行研究と重なる
  ことが分かったため)。
- **タイトル案**:
  - "When Does Adaptive Standby Switching Pay Off? Closed-Form Boundaries and a
    Real-Trace Case Study for Dual-Hop Relay Routing over Markov-Modulated Channels"
  - (短縮版) "Warm/Cold Standby Boundaries for Dual-Hop Relay Routing: Theory and a
    Real-World Negative Result"

## 目次案

### 1. Introduction
- 1.1 動機: 2ホップのモバイルリレー(isekai-terminalのQUIC path-validation文脈)、
  warm standby(常時probe、`c_warm`/step) vs cold standby(probeなし、復帰時に
  `c_switch_cold`)のトレードオフ
- 1.2 応用上の問い: 実チャネル統計のもとで、belief-basedなadaptive切り替えは、
  単純な固定戦略(常時warm/常時cold)に対してどれだけの価値を持つか
- 1.3 貢献(**正直にスコープを絞って書く**):
  1. 任意のチャネル・コスト構造で成り立つ一般的な構造的上界 `Φ≤c_warm`
     (証明技法自体はPOMDP文献で標準的な「相手の最適policyを模倣する」議論だが、
     この非対称switching-cost設定での定式化と、taut性の説明は新しい)
  2. 決定論損失(pure-Gilbert)という理想化のもとでの、warm/cold境界の完全閉形式解
  3. `pi_b>1/2`での非単調性の2コンポーネント拡張とその閉形式閾値
     (**単一コンポーネント版の閾値条件自体は2024年のAoI文献に先行例あり**、
     ここは新規性を強く主張しない)
  4. **Berlin V2X実データでの検証**: 較正済み運用点が、実測した較正不確実性のもとでも
     頑健に低gain(<1%)な領域にあることを示し、「なぜadaptiveな価値が実測で小さいか」
     に初めて解析的な説明を与えた
- 1.4 関連研究への簡単なポインタ(過大な新規性主張を避けるため、早い段階で§2に誘導)

### 2. Related Work
- 2.1 Gilbert-Elliottチャネル上のPOMDP/restless bandit scheduling
  (Ahmad, Liu, Javidi, Zhao, Krishnamachari 2009; Zhao, Krishnamachari, Liu 2008;
  Meshram, Manjunath, Gopalan 2018; Liu & Zhao 2010) — 使用するチャネルモデルと
  還元技法の出典
- 2.2 Restless bandit / 構造化POMDPにおけるswitching cost
  (Jun 2004; Glazebrook, Ruiz-Hernandez, Kirkbride 2006; Banks & Sundaram 1994;
  Krishnamurthy & Djonin 2007; Krishnamurthy structural results) — switching cost
  機構と「相手のpolicyを模倣する」議論(genie-aided bound)の出典
- 2.3 **Wang, Nazarathy & Taimre (2021)** — 全観測 vs 部分観測のチャネル選択、
  switching cost込み。**§4のΦ設定に最も近い先行研究**。両者の違い
  (彼らは両レジームの閉形式を別々に出すのみ、こちらは優越性論法で一般的な上界を出す)
  を明示的に論じる節を設ける
- 2.4 **arXiv:2403.03380 (2024), "On the Monotonicity of Information Aging"** —
  マルコフ連鎖の非単調エイジングの閾値条件(`pi_b`と1/2の比較)について、
  **単一コンポーネント版で完全に同一の分水嶺条件を先に確立**している。
  **§5.3の新規性主張の範囲を正確に画定するために、正面から比較する節が必須**
- 2.5 単調性を「証明しようとする」側の対比文献(arXiv:2601.19131) — 分野のデフォルトの
  期待が単調性であることを示す文脈として、非単調な反例の価値づけに使う
- 2.6 連続時間最適switching(Ly Vath & Pham 2007)とwarm/cold standby信頼性工学
  (Levitin, Xie 系列) — 別系統のモデリング伝統として簡潔に触れる

### 3. Model and Problem Formulation
- 3.1 独立な2つのGilbert-Elliottホップ、path A(固定コスト) vs path B(状態依存損失)
- 3.2 Warm standby: belief-based POMDP、always-warmサブモデル
- 3.3 Cold standby: blindなpark-and-return、always-coldサブモデル
- 3.4 Adaptive policyと2つの問い: (a) warmはc_warmを払う価値があるか (b) 固定戦略は
  adaptiveの最適に対してどれだけ劣るか

### 4. A General Structural Bound: Φ≤c_warm
- 4.1 「相手の最適policyを模倣する」実行可能候補の構成
- 4.2 定理と証明(`c_switch_cold≥c_switch_warm`という物理的に常に成り立つ条件込み)
- 4.3 taut性(switch率0の退化点でのみ等号)と、§2.3との比較

### 5. Closed-Form Analysis under the Pure-Gilbert Idealization
- 5.1 決定論的観測によるbeliefの単純化(有限MDP/semi-Markov還元)
- 5.2 Warm側の3つの閉形式policy
- 5.3 Cold側の閉形式と`pi_b>1/2`の非単調性
  - 5.3.1 現象の記述とメカニズム(増加因子×減少因子の積)、**§2.4との正面比較**
  - 5.3.2 2コンポーネントへの拡張と閉形式脱離閾値
  - 5.3.3 スコープの明示: `pi_b<=1/2`が「通常」領域であり、§6で実データがそちらに
    属することを示す(=このコーナーケースが実際に効くのは稀、という誠実な位置づけ)
- 5.4 `pi_b<=1/2`(主要ケース)での`Φ=0`境界閉形式

### 6. Real-World Validation: A Berlin V2X Case Study
- 6.1 データとGEパラメータ較正(EM/Baum-Welch)、方法論
  (2回の誤った初期結論とその訂正過程を「教訓」として簡潔に記載し、較正の信頼性を担保)
- 6.2 較正済み運用点は`Φ=0`境界に対してどこにあるか
- 6.3 直接測定したadaptive gain(vs best-fixed): 0.65%
- 6.4 較正不確実性へのロバストネス(想定ではなく、ブートストラップで実測した不確実性で
  再検証。当初の想定幅が実は保守的でなかったことも含め、正直に報告)
- 6.5 独立エージェントによる数値の再現検証

### 7. Discussion
- 7.1 実システム設計への含意(adaptive切り替えを実装しない、という設計判断の根拠)
- 7.2 結論が反転する条件(`c_warm/cost_a`が大きいシステムでは話が変わる、という条件文)
- 7.3 誠実な限界: pure-Gilbert理想化 vs 一般のeps、対称 vs 非対称ホップ、
  `pi_b>1/2`結果の適用範囲の狭さ

### 8. Conclusion

### Appendices
- A. 完全な導出(sympyスクリプトへの参照、`WARM_COLD_PURE_GILBERT_NOTES.md`の該当箇所)
- B. 追加のロバストネス掃引
- C. 方法論的な教訓(グリッド解像度アーティファクト、EM fittingの落とし穴) —
  reproducibility/credibilityのための正直な失敗談セクション

## 執筆上の優先順位(この構成案の意図)

1. **§6(実データ検証)に最も紙幅を割く**。ここが最も検証されており、最も守りやすい
   主張。
2. **§2(関連研究)を薄くしない**。特に2.3・2.4は「読んで正面から比較」が必須で、
   これを怠ると査読で確実に指摘される。
3. **§4・§5の理論部分は「なぜ§6の結果が構造的に説明できるか」という補助線として
   書く**。「新しい定理を発見した」ではなく「実測結果を理解するための道具を用意した」
   というトーン。
4. **§7.3で限界を先に自分から言う**。査読者に指摘される前に書いておく方が印象が良い。
