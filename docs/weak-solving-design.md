# NOCCA×NOCCA 弱解決(weak solving)設計

> **歴史資料**: これはプロジェクト初期の **前向き弱解決(df-pn)** アプローチの設計書である。
> draw が多い NOCCA の局面で証明数が発散したため、最終的には **後退解析による強解決
> (全局面の値+DTMを計算)** に移行し、それが 6×5 の完全な結果を生んだ(README 参照)。
> 本書は当時の設計として保存している。実装の名残は `src/solver.rs`。

ステータス: 承認済み設計(実装はこれから)。実装は段階A〜C(正しさ検証)→ 初期局面、の順で進める。

強解決(全状態の in-RAM 列挙)は状態数実測で非現実的と確定したため、**初期局面の勝敗を前向き証明探索で出す弱解決**に絞る。本書は型シグネチャ案まで含む実装指針。

前提となる既存 API(`nocca` クレート):
- `Position`(手番側正規化・`turn` フィールドなし・`Copy`)、`Position::generate_legal_moves(&mut MoveList)`、`Position::apply_move(Move) -> Position`(reorient込み)、`Position::opponent_reached_goal() -> bool`、`Position::canonical_key() -> u128`(mirror畳み込み済み)。
- `MoveList`(固定容量・アロケーションなし)。

---

## 0. 解くべき「値」のセマンティクス(確定)

対局用の「3回反復=引き分け」ルールとは**別に**、証明のセマンティクスを次に固定する:

> **「無限に続けられる手順 = 引き分け(perpetual avoidance = draw)」**
> 手番側(= `US` = 黒)視点で局面 `P` の値を 3 値で定義する:
> - **Win**: 手番側が**有限手で**相手ゴール到達を強制できる。
> - **Loss**: 相手が手番側の負け終局(相手ゴール到達 or 手番側に合法手なし)を強制できる。
> - **Draw**: どちらも自分のゴール到達を強制できない(= 永久に避けられる)。

この値は**2つの到達可能性ゲーム(reachability game / attractor)の解**であり、**経路独立(history-free)**。各局面に一意で、`canonical_key` をキーにしたキャッシュが健全になる。これが GHI 問題(§2)を回避する土台。

**非対称な現実(覚悟)**: 初期局面が **Win/Loss なら証明木サイズで済む**。**Draw なら「初期から両者ゴール強制不可」を示す = 到達可能な非決着領域の全探索**に等しく、強解決級に重い。ランダム対局がほぼ五分(White 50.7%)なので Draw も十分あり得る。

---

## 1. 値の型と 3 値 negamax

```rust
// solver/value.rs
//
// 手番側視点。順序は Loss < Draw < Win(derive(Ord) がこの宣言順を採用)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Value {
    Loss,
    Draw, // 「引き分け、または(深さ制限時は)未証明」。Win/Loss だけが“証明”。
    Win,
}

impl Value {
    /// 子(相手手番)の値を親(自分手番)視点へ反転。Win<->Loss, Draw->Draw。
    #[inline]
    pub fn negate(self) -> Value {
        match self {
            Value::Win => Value::Loss,
            Value::Loss => Value::Win,
            Value::Draw => Value::Draw,
        }
    }
}
```

negamax 本体(疑似):

```
value(P, ply, alpha, beta):
    # 1. 終局(経路独立)
    if P.opponent_reached_goal():           return Loss          # 既に負け
    P.generate_legal_moves(ml)
    if ml.is_empty():                        return Loss          # 規則B
    # 2. TT は“証明”だけを返す(W/L)。経路に依存しないので最優先で採用。
    if let Some(p) = tt.probe(P.canonical_key()): return p.into() # Win or Loss
    # 3. パス上の反復 = Draw(2-fold)。TT 証明が無い場合のみ。
    if path.contains(P.canonical_key()):     return Draw
    # 4. 展開
    path.insert(P.canonical_key())
    best = Loss
    for m in ml:
        v = value(P.apply_move(m), ply+1, beta.negate(), alpha.negate()).negate()
        best = max(best, v)
        alpha = max(alpha, v)
        if v == Win or alpha >= beta:        break                # Win 即カット
    path.remove(P.canonical_key())
    # 5. 証明できた W/L だけ TT へ。Draw は格納しない(§2.3)。
    if best == Win or best == Loss:          tt.store(P.canonical_key(), best, work)
    return best
```

- αβ窓は `{Loss, Draw, Win}` 上。主たる枝刈りは「子が Loss → 親 Win 即 return」。
- **`Win`/`Loss` だけが証明**。`Draw` は「反復による引き分け、または深さ制限による未証明」を意味する(§7)。

---

## 2.【最重要】ループ処理と GHI の正しさ

### 2.1 パス反復検出

- DFS の**現在パス上**の局面を `HashSet<u128>`(`canonical_key`)で保持。入口で挿入・出口で削除。
- 子の `canonical_key` がパス集合に既出なら **2-fold で即 `Draw`**(無限循環可能 = 避ける側に少なくとも引き分け)。
- `canonical_key` を使うので鏡像は同一状態として正しく反復扱い。

### 2.2 3 値 min/max への組み込み

- 親 = 子の `negate()` の最大。子に `Loss`(= 親 `Win`)があれば親 Win。全子が `Win`(= 親 `Loss`)なら親 Loss。それ以外は `Draw`。
- 反復枝は `Draw` として合成に寄与。Loss にも Win にも“偽装しない”(§1の負号規則)。

### 2.3 GHI 健全化ルール(本設計の核)

`canonical_key` は履歴を持たないが、**反復由来の Draw は履歴依存**。これを TT に exact 格納して別経路で再利用すると、本来 Win/Loss の局面を誤判定する(Graph History Interaction)。処方:

> **TT に格納してよいのは「証明済み `Win`」と「証明済み `Loss`」のみ。`Draw` は決して格納しない。**

**健全性(定理)**: negamax の上記定義の下で、ノードが `Loss` を返すのは「全子が `Win`」または終局時のみ。`Draw`/未証明子が一つでもあれば `Loss` にならない。よって `Loss` は反復にも深さ制限にも依存しない真の証明。`Win` は `Loss` の子に依存するので同様に真の証明。∎
→ ゆえに **TT に入る W/L は常に経路独立**で、再利用も置換も健全。`Draw` を入れない限り GHI は起きない。

**TT 証明 vs パス反復の優先順位**: §1 の通り **TT(W/L)を先に採用**。ある局面が証明済み Win なら、たとえパス上にあっても Win(勝てる側は反復に逃げない)。健全。

### 2.4 αβ / PNS はそのまま使えるか

- **αβ**: 3 値ミニマックスとして使える。**修正点 = (i) パス反復→Draw、(ii) TT は W/L のみ格納(§2.3)**。これを欠くと Draw で不健全。
- **PNS/df-pn**: 本来二値。引き分けは「Win 証明」と「¬Loss 証明」の**2回探索**等で対応。さらに**巡回 DAG + TT では df-pn(r)(Kishimoto–Müller)等の反復しきい値制御で GHI を安全化**しないと不健全。

---

## 3. エンジン選択(αβ + TT を基盤、df-pn を加速器)

| | αβ+TT(3値・反復深化) | df-pn(r) |
|---|---|---|
| 引き分け | 第3値として自然 | 2回探索 or draw対応変種が必要 |
| 正しさ検証 | 容易(§9) | 巡回GHI対策で難度↑ |
| 速度 | 一様・堅実 | 非一様木で勝ち証明が速い |
| メモリ | W/Lのみ格納。置換は速度のみに影響 | TTで省メモリだが反復制御要 |

**方針**:
1. **基盤 = negamax/αβ・3値・反復深化・TT(W/L のみ)** をまず実装し正しさを固める。
2. 初期局面が決着していそうなら **df-pn(r) を W/L 証明の加速器として追加**(ノッカは乗っかりで分岐が非一様 → 資源集中が効く)。

---

## 4. トランスポジション表

```rust
// solver/tt.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Proven { Win, Loss }                 // 経路独立な結果のみ

impl From<Proven> for crate::solver::Value {  // probe結果を Value へ
    fn from(p: Proven) -> Self { match p { Proven::Win => Value::Win, Proven::Loss => Value::Loss } }
}

#[derive(Clone, Copy)]
struct TtEntry {
    key: u128,
    proven: Proven,
    work: u32,   // 部分木サイズ(置換の優先度に使用)
}

pub struct Tt {
    table: Box<[Option<TtEntry>]>, // 2の冪・オープンアドレッシング
    mask: usize,
    filled: usize,
    probes: u64,
    hits: u64,
}

impl Tt {
    /// メモリ上限から容量(2の冪エントリ数)を決めて確保。
    pub fn with_capacity_bytes(bytes: usize) -> Self;
    pub fn probe(&mut self, key: u128) -> Option<Proven>;
    pub fn store(&mut self, key: u128, proven: Proven, work: u32);
    pub fn len(&self) -> usize;          // 占有エントリ数(進捗出力用)
    pub fn capacity(&self) -> usize;
}
```

- **キー**: `canonical_key`(u128, mirror畳み込み済み)。値は mirror 不変なので健全。
- **格納**: `Win`/`Loss` のみ。`Draw` は格納しない(§2.3)。
- **置換戦略は正しさに無関係**(W/L は事実。追い出しても再探索で再導出されるだけ)。速度のため **2レベル(work/depth-preferred + always-replace)** を推奨。`--tt-bytes` で上限。溢れても淘汰のみで健全。

---

## 5. 反復検出のパス集合

```rust
// solver/path.rs
/// 現在の DFS パス上の canonical_key 集合。入口 push / 出口 pop。
pub struct PathSet {
    on_path: std::collections::HashSet<u128>,
}

impl PathSet {
    pub fn new() -> Self;
    pub fn contains(&self, key: u128) -> bool;
    /// 既に在ればフレームを作らず false(= 反復)。新規なら挿入して true。
    pub fn enter(&mut self, key: u128) -> bool;
    pub fn leave(&mut self, key: u128);
}
```

---

## 6. ソルバ API と反復深化(進捗出力)

```rust
// solver/mod.rs
pub struct Solver<'a> {
    tt: &'a mut Tt,
    path: PathSet,
    max_ply: u32,   // この反復の深さ上限。超過ノードは Draw(=未証明)を返す(§7)。
    nodes: u64,
}

impl<'a> Solver<'a> {
    pub fn new(tt: &'a mut Tt, max_ply: u32) -> Self;
    /// 手番側視点の値。深さ制限つきなら Win/Loss は証明、Draw は未証明。
    pub fn search(&mut self, pos: &Position) -> Value;
    pub fn nodes(&self) -> u64;
}

/// 反復深化の1イテレーション分の進捗(逐次出力用)。
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub max_ply: u32,
    pub root_value: Value, // Win/Loss=確定、Draw=この深さでは未確定
    pub nodes: u64,
    pub tt_entries: usize,
    pub elapsed_secs: f64,
}

/// 根を ply 制限つき反復深化で解く。各 ply 後に on_progress を呼ぶ。
/// Win/Loss が確定したら早期 return。max_ply_limit まで未確定なら Draw を返す
///（= この上限内では未証明。真の Draw 証明には §7 の不動点が必要）。
pub fn solve_root<F: FnMut(&Progress)>(
    root: &Position,
    tt: &mut Tt,
    max_ply_limit: u32,
    mut on_progress: F,
) -> Value;
```

**CLI**(`src/bin/solve.rs`):
```
cargo run --release --bin solve -- [--max-ply L] [--tt-bytes B]
```
各 ply 反復後に1行ずつ出力(ご指定の逐次進捗):
```
ply   root_value   nodes        tt_entries   elapsed
  1   draw         123          0            0.0s
  2   draw         3,420        45           0.0s
 ...
  k   WIN          12,345,678   2,103,556    87.3s   <- 確定で停止
```
途中で打ち切っても、**到達 ply・根の暫定値・TTサイズ・経過時間**が分かる。

---

## 7. 深さ制限の健全性(段階4の補正)

- `max_ply` 超過ノードは `Draw`(=未証明)を返す。
- **深さ制限探索は `Win`/`Loss` の証明には健全**(制限内に強制勝ち/負けが見つかればそれは本物。§2.3の定理は深さ制限下でも成立 — 超過の `Draw` は `Loss`/`Win` を生成しない)。
- **「制限到達 = 引き分け」と結論するのは証明ではない**。`Draw` を**証明**するには:
  - (a) **不動点検出**: ply を増やしても新規 `Win`/`Loss` が一切増えない、かつ根が `Draw` に留まる、または
  - (b) 到達可能な非決着領域の全探索(= 高コスト、強解決級)。
- 例外: **小さい閉じた局面で深さ制限に一切触れず探索が完了した場合**、返る `Draw` は真の引き分け(局所不動点)。段階A〜Cのテストはこれを利用する。

---

## 8. 実装マイルストーン(この順で)

> **初期局面は最後**。重い/引き分けで終わらない可能性があるため、小さい既知局面で正しさを固めてから根に向かう。

1. **M1: 値・TT・パス集合・negamax の骨格**(§1,4,5)。`solver` モジュール新設。
2. **M2: 段階A 通過** — 手計算可能なマイクロ局面の値一致(§9-A)。
3. **M3: 段階B 通過** — TTなし総当たりミニマックスとのクロス検証(§9-B)。
4. **M4: 段階C 通過(GHI 罠)** — 経路独立性の専用テスト(§9-C)。**ここまでが第1マイルストーンの完了条件**。
5. **M5: 反復深化ドライバ + `solve` バイナリ + 逐次進捗出力**(§6)。
6. **M6: 初期局面へ反復深化**。決着すれば確定、しなければ不動点 or メモリ壁を報告。
7. **M7(任意): df-pn(r) 加速器**。

---

## 9. 検証テスト

### 段階A — マイクロ局面(値が自明)
- US が1手でゴール行へ → `search == Win`。
- 相手が次に1手でゴール(US は阻止/回避不能) → `Loss`。
- 相互に避け合う対称局面(誰もゴール強制不可) → `Draw`(深さ制限に触れず完了する小局面)。

### 段階B — 総当たりとのクロス検証
- コマ数の少ない小局面で、**TTなし・パス反復=Draw の素朴3値ミニマックス**(`fn brute(pos, ply_limit) -> Value`)を別実装。
- 同じ局面群で **TT版 `Solver::search` と完全一致**を assert。TT/GHI 実装のバグ検出に最有効。

### 段階C — GHI 罠(必須)
- **構成**: 手番側に2手ある局面 `P` を作る。手 X は数手で強制勝ち(Win)へ、手 Y は祖先へ戻る(反復 = Draw)。
- **期待**: `search(P) == Win`(経路に Draw 枝があっても、経路独立な Win を返す)。
- **対照**: 「Draw も TT 格納する素朴版」なら、先に Y 経由で `P` を Draw 確定 → 別経路で誤って Draw 再利用、で誤答することをコメントで明示(本実装は Draw を格納しないので回避)。
- 追加: 「非反復手はすべて Loss、反復手だけが避け道」の局面 → `Draw`(避けで負けを回避できるが勝てない)。

### 常時
- 出力とランダム対局統計の整合(White 強制勝ちなら矛盾しないか。ただし統計から Draw は結論不可)。

---

## 10. 既知の限界

- 初期局面が **Draw** の場合、弱解決でも証明コストは強解決級になりうる(§0)。`solve` は不動点 or メモリ壁に達したら**進捗・到達 ply・見通し**を出力して止める設計とする。
- df-pn(r) は強力だが巡回 GHI 安全化が前提。導入は M4 まで(αβで正しさ確立)後。

---

## 11. df-pn(r) 加速器の詳細設計

αβ反復深化の実測(ply13 = 40億ノード/600秒・root依然 draw・facts件数わずか1654)で、
**根付近は W/L がほぼ証明されず TT が効かず、素のαβは非現実的**と確定。df-pn(r) へ移行する。

### 11.1 引き分け対応 = 2回の二値探索(採用)

- **探索1**: 「先手番側(P=root mover)は Win を強制できるか」を df-pn(r) で証明。
- **探索2**(探索1が反証された場合のみ): 「相手(P=opponent)は root mover の Loss を強制できるか」。
- 根の判定: 探索1証明→**Win** / 探索2証明→**Loss** / 両方反証→**Draw**。

理由: 各二値 df-pn(r) は公刊アルゴリズムをそのまま使え、**引き分け・反復はどちらの探索でも
「反証(disproof)」葉に潰れる**ため探索内に第3値処理が不要。巡回GHI安全化(Kishimoto-Müller)の
二値理論をそのまま適用できる。draw対応PN変種(b)は3値PNと巡回安全化を同時に正しく扱う必要があり、
最優先の正しさのリスクが高いので不採用。

### 11.2 巡回GHI安全化

**(i) 反復 = 即時反証葉**: DFSパス上(`HashSet<u128>` of `canonical_key`)に既出の局面へ来たら、
証明中の問いに対し即 `Pd{pn:∞, dn:0}` を返す。back-edge がそこで閉じるので**無限再帰せず停止**。

**(ii) 証明のみ恒久化(taint不要)**:
> **定理**: df-pn の「証明」探索で `pn=0`(証明)になった部分木は必ず反復非依存。
> 反復葉は `pn=∞` の反証。OR節の証明は子に `pn=0` を要し、AND節の証明は全子 `pn=0` を要するので、
> `pn=∞` の反復葉は証明に寄与できない。∴ `pn=0 ⇒ 反復非依存 ⇒ 経路独立`。

よって **`pn=0` 確定ノードだけを §4 の facts TT(`Tt`、Win/Loss)へ昇格**し、
反復依存しうる**反証(dn=0)は恒久化しない**。これでGHI(別経路での誤再利用)を構造的に回避。
根は path 文脈が空なので最終判定(`dn=0`=¬Win=Loss/Draw)は健全。

**facts TT の共有**: 既存 `Tt` を探索1・探索2・αβ で共有。各ノードでまず facts を引き、Win/Loss なら
ノード型(OR/AND)に応じ proof/disproof 葉へ変換。(pn,dn) 作業表は探索ごとに別建て(溢れたら淘汰=遅く
なるだけで健全)。

### 11.3 ノード(明示的 OR/AND・ply偶奇)

正規化表現は手番フィールドが無いので、証明者 P が手番かを **ply 偶奇**で判定:
- 探索1(P=先手番側): OR=偶数ply、AND=奇数ply。 探索2(P=相手): OR=奇数ply、AND=偶数ply。
- 葉: 終端(`opponent_reached_goal` か合法手なし)でノード手番が負け → 手番が P なら反証、相手なら証明。
  反復 → 反証。
- 伝播(飽和加算): OR節 `pn=min子pn, dn=Σ子dn` / AND節 `pn=Σ子pn, dn=min子dn`。

### 11.4 型シグネチャ案

```rust
// solver/dfpn.rs
const INF: u32 = u32::MAX;

#[derive(Clone, Copy)] enum Goal { ProveWin, ProveLoss }
#[derive(Clone, Copy)] struct Pd { pn: u32, dn: u32 } // proof/disproof numbers

struct WorkTable { /* canonical_key -> Pd, 固定長・淘汰可 */ }

pub struct Dfpn<'a> {
    facts: &'a mut Tt,          // 既存§4: 証明済み Win/Loss(共有)
    work:  WorkTable,           // (pn,dn) この探索専用
    path:  std::collections::HashSet<u128>,
    goal:  Goal,
    nodes: u64,
}
impl<'a> Dfpn<'a> {
    fn is_or(&self, ply: u32) -> bool;
    fn leaf(&mut self, pos: &Position, ply: u32) -> Option<Pd>; // 終端/facts/反復
    fn mid(&mut self, pos: &Position, ply: u32, th: Pd) -> Pd;  // df-pn MID(しきい値制御)
    pub fn prove(&mut self, root: &Position) -> bool;          // 根 pn==0 で true
}

/// 2探索で初期局面の3値を返す。
pub fn solve_dfpn(root: &Position, facts: &mut Tt, work_bytes: usize) -> Value;
```

根ドライバ:
```
solve_dfpn(root):
    if Dfpn::new(facts, ProveWin).prove(root):   return Win
    if Dfpn::new(facts, ProveLoss).prove(root):  return Loss
    return Draw
```
`bin/solve` に `--engine dfpn` を追加。進捗は探索ごとに根の `(pn,dn)`・nodes・facts件数・経過を逐次出力。

### 11.5 検証の最初の関門

既存の段階A〜C を **`solve_dfpn` でも緑**にする(同じ `Value` を返し同テストを流用):
- 段階A: 1手勝ち=Win / 相手強制=Loss / **安定引き分け=Draw**(= df-pn(r)が引き分けで停止する直接テスト=巡回安全化の本丸)。
- 段階B: brute oracle との含意(df-pnは真値=無制限、brute決着⇒一致)。
- 段階C: **GHI罠**(共有facts TT+warm再実行で経路独立性)。これが df-pn 版の合格条件。

### 11.6 限界と実装の現状(重要・実測反映)

**実装で判明したこと(M7a/M7b)**:
- 健全性(soundness)は成立: df-pn は**決して誤った勝敗を返さない**。証明済み facts は常に正しく、
  warm でも cold でも oracle と矛盾する決着を出さない(段階B/Cで検証・緑)。
- ただし**素の df-pn は本ゲームの巡回構造で頻繁に「不完全」**: (pn,dn) が巡回領域で収束せず、
  proof/disproof に届かず**スラッシング**する。fresh facts でも一部の2駒局面で発生(例: brute が
  Loss と決着する局面で df-pn は到達できず)。warm facts は探索順を変えてスラッシングを誘発しうる。
- そこで**ノード予算(`--node-limit` / `with_node_limit`)で必ず停止**させ、予算超過は「未証明」
  = 不確定 `Draw` として返す。これにより**健全かつ必ず停止**するが、**深い巡回局面では不完全**
  (本当は Win/Loss でも `Draw` を返しうる)。

**含意**:
- df-pn が**きれいな強制勝ちを持つ局面**では proof を効率的に発見(例: 1手勝ち近傍は数百ノード)。
- 巡回が絡む disproof(特に perpetual 防御=引き分けや、相手が肉薄する Loss 証明)は**収束しない**
  ことが多く、予算で打ち切られ不確定になる。
- **完全性の本質的な解決には、本来の df-pn(r)**(Kishimoto–Müller の threshold-controlled
  repetition / source-node・delta-stamp による work-table の GHI 安全化)**が必要**。これは
  research-grade の追加実装で、別マイルストーンとする。

まず探索1(Win証明)を fresh facts で初期局面に回し、決着するか/スラッシングで打ち切られるかを観測する。

### 11.7 実装マイルストーン

- **M7a**(済): `Dfpn`(MID・leaf・OR/AND・反復反証・facts昇格・ノード予算)+ `solve_dfpn`。
- **M7b**(済): 段階A〜C を df-pn 版で緑。**健全性**を関門に(完全性は素の df-pn では未達)。
- **M7c**(済): `bin/solve --engine dfpn` と pn/dn・nodes・facts・経過の逐次出力。
- **M7d**: 初期局面へ(探索1=Win証明、fresh facts)。決着 or スラッシング打ち切りを報告。
- **M8(未・要判断)**: 完全な df-pn(r)(threshold-controlled repetition / work-table GHI 安全化)。
  巡回局面の完全性が必要なら実装。research-grade。
