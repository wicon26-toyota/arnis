# AGENTS.md

このリポジトリで作業する AI エージェント（および人間のコントリビューター）向けのガイドです。
作業前に `README.md`、`Cargo.toml`、`.github/workflows/ci-build.yml` も確認してください。

## プロジェクト概要

- Arnis は OpenStreetMap と標高データから、実在する街並みを Minecraft のワールドとして生成する Rust 製ツールです。
- Java Edition / Bedrock Edition / Luanti（Minetest）に対応しています。
- GUI は Tauri 2（`gui` feature、デフォルト有効）。CLI は `--no-default-features` でビルドします。
- このチェックアウトは `wicon26-toyota/arnis` の `feature/jegg500-bathymetry` ブランチです。日本周辺の J-EGG500 海底地形、補間、回転対応、GUI の範囲表示などが upstream に対する主な追加点です。
- ライセンスは Apache-2.0 です。`src/luanti_block_map.rs` の LGPL ヘッダと帰属表示は保持してください。

## クイックコマンド

```bash
# 型チェック（最短）
cargo check

# GUI 込みビルド / 実行
cargo build --release
cargo run --release

# CLI のみ（GUI のシステム依存を避ける）
cargo build --release --no-default-features
cargo run --release --no-default-features -- \
  --output-dir="$HOME/.minecraft/saves/test" \
  --bbox="35.55,139.65,35.75,139.90"

# テスト
cargo test --all-targets --all-features   # CI と同じ。GUI のシステム依存が必要
cargo test --no-default-features          # GUI 依存なしで速い
cargo test <test_name>                    # 単体テストを絞る

# Lint / format（CI は両方必須）
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

- Linux で GUI をビルドするには GTK / WebKitGTK などの開発パッケージが必要です。詳細は README の「必要な環境」を参照してください。
- Nix では `nix build` / `nix run . -- <args>` が使えます。
- CI は stable toolchain で `fmt --check`、`clippy --all-targets --all-features -- -D warnings`、`build --all-targets --all-features --release`、`test --all-targets --all-features` を実行します。

## 処理の流れ

```text
main.rs / gui.rs
  -> args.rs（CLI 引数・検証）
  -> retrieve_data.rs（Overpass / ローカル .osm / 保存）
  -> osm_parser.rs（タグ整理・ProcessedElement へ変換）
  -> coordinate_system / map_transformation（投影・回転・移動）
  -> ground / elevation / land_cover（地形・地表）
  -> data_processing.rs（タイル分割・並列処理）
       -> element_processing/*（建物・道路・水域など）
       -> structures / trees / models_3d / decals
       -> water_depth / jegg500（水深）
  -> world_editor/*（Java Anvil / Bedrock / Luanti へ書き出し）
```

### 主要モジュール

| パス | 役割 |
| --- | --- |
| `src/main.rs` | CLI のエントリ。引数検証、前処理、生成全体のオーケストレーション |
| `src/gui.rs`, `src/gui/` | Tauri コマンド、HTML/JS/CSS、ロケール |
| `src/args.rs` | clap の引数定義とバリデーション |
| `src/retrieve_data.rs` | Overpass からの取得、ローカル `.osm` の読み込み、JSON 保存 |
| `src/osm_parser.rs` | OSM データの解析、タグの取捨選択、XZ 座標への変換 |
| `src/overture/` | Overture Maps の建物データ（PMTiles / Parquet）とディスクキャッシュ |
| `src/coordinate_system/` | `LLBBox` / `LLPoint`（WGS-84）、`XZBBox` / `XZPoint`、`CoordTransformer` |
| `src/map_transformation/` | 回転・平行移動・`transform_map` |
| `src/elevation/`, `src/elevation_data.rs` | 標高プロバイダの選択、キャッシュ、補正・後処理 |
| `src/land_cover/` | 土地被覆、OSM の水域・陸地上書き、海岸線、橋の補修 |
| `src/ground.rs`, `src/ground_generation.rs` | 地表・地形レイヤの生成 |
| `src/element_processing/` | OSM 要素ごとの生成処理（建物、道路、鉄道、自然物など） |
| `src/water_depth.rs`, `src/jegg500.rs` | 従来の水深生成と J-EGG500 海底地形 |
| `src/structures/`, `src/trees/`, `src/models_3d/`, `src/decals/` | 構造物、樹木、3D モデル、看板・ポスター |
| `src/tile.rs`, `src/data_processing.rs` | タイル分割、要素の割り当て、並列生成 |
| `src/world_editor/` | Java Anvil / Bedrock `.mcworld` / Luanti `map.sqlite` の書き出し |
| `src/voxy/` | Voxy mod 向け LOD キャッシュ（Java のみ） |
| `src/deterministic_rng.rs` | 要素 ID・座標から決まる再現可能な乱数 |
| `src/net.rs` | HTTP リクエストの同時実行数制限 |
| `src/block_definitions.rs` ほか | ブロック定義と Java / Bedrock / Luanti の変換 |

## 変更時に守るルール

### 座標系

- 地理座標は `LLBBox` / `LLPoint`、Minecraft のローカル座標は `XZBBox` / `XZPoint` です。両者の変換は `CoordTransformer` に集約してください。
- 回転は `map_transformation::rotate` でワールド全体に適用されます。J-EGG500 は「回転前の格子」を保持し、`BathymetryGrid::sample_depth_m` が最終座標を逆回転して参照します。新しいデータ源もこの方式に合わせてください。
- 緯度経度とブロック座標を直接混ぜないでください。`args.scale` や ground level を変更する場合は、前処理から書き出しまで一貫させます。

### 決定性

- 生成結果に使う乱数は `deterministic_rng::{element_rng, coord_rng}` を使います。`thread_rng` や `rand::random` をワールド生成に使わないでください。
- 同じ入力からは、タイル分割・並列実行・要素の処理順が変わっても同じワールドができる必要があります。
- 出力順に影響する場所で `HashMap` / `HashSet` の反復順に依存しないでください。必要ならソートするか `BTreeMap` を使ってください。同点時の tie-break も明示します。
- `sort_ground_fill_areas` のような描画順の不変条件を壊さないでください。

### タイル並列とメモリ

- `data_processing.rs` は 512×512 ブロックのタイルを rayon で並列処理します。共有状態は不変参照にし、可変状態はタイルごとの `WorldEditor` に閉じ込めてください。
- 要素は複数のタイルにまたがります。`TILE_EDITOR_HALO` の前提（建物や滑走路などの最大半幅）を壊す変更を入れないでください。
- 大きな bbox を扱います。配列サイズは `checked_mul` と既存の `MAX_*` 上限で守り、無制限の `Vec` 確保を追加しないでください。
- 性能とピークメモリは CI のベンチマークで比較されます。ホットパスに不要な `clone` / `collect` / `format!` を追加しないでください。既存の f32 グリッド、キャッシュ、ディスクストリーミングを活用します。

### エラーとフォールバック

- ユーザー入力や外部データで panic しないでください。`Result` を返し、呼び出し側で文脈を付けたエラーにします。
- 外部データが欠けても生成を続けます。J-EGG500 がなければ従来の水深生成またはフラットな水、Overture が失敗しても OSM だけで生成、標高プロバイダが失敗したら次の候補を試す、という既存の挙動を維持してください。
- 「欠損」と「破損」は区別します。J-EGG500 の実データテストはデータがないとき skip、読めたが壊れているときは fail です（`src/jegg500.rs` の `load_real_jegg500` 参照）。
- プロバイダ系の HTTP 呼び出しは `src/net.rs::request_permit` で全体の同時実行数を制限します。タイムアウトとリトライも設定してください。

### GUI と CLI の一貫性

- 生成オプションを追加するときは、CLI と GUI の両方に配線します。
  - `src/args.rs`
  - `gui_start_generation` の引数（`src/gui.rs`）
  - `src/gui/index.html`
  - `src/gui/js/main.js`
  - `src/gui/js/settings-store.js`
  - `src/gui/locales/{en-US,en,ja}.json`
- Tauri コマンドを追加したら `src/gui.rs` の `generate_handler!` に登録します。
- ロケールは `en-US.json` を基準にキー集合を揃えます（`locale_tests` が検査）。日本語 UI も忘れず更新してください。
- GUI と CLI でデフォルト値・バリデーション・生成結果がずれないようにしてください。

### データ・ライセンス・生成物

- `jegg500/` は J-EGG500 の生データ置き場で、`.gitignore` 対象です。**コミット・再配布しないでください。** テストもデータがなければ skip するのが正常です。
- `target/`、`gen/`、`/wiki`、`*.mcworld`、各種キャッシュ、`parsed_osm_data.txt`、デバッグ用 PNG などは生成物です。コミットしないでください。
- `Cargo.lock` はコミット対象です。依存を変更したときだけ意図的に更新してください。
- `assets/` の schematic・画像・テンプレートは実行時に埋め込まれます。バイナリ資産を置き換えるときは理由と出典をコミットメッセージに書いてください。
- `src/luanti_block_map.rs` の LGPL ヘッダと帰属表示を消さないでください。

### コードスタイル

- `cargo fmt` のデフォルト設定に従います。Clippy 警告は CI で `-D warnings` により失敗します。
- コメント・doc comment は既存コードに合わせて英語、README / AGENTS.md は日本語で書きます。
- 変更は最小限にしてください。無関係な整形・リネームを混ぜないでください。
- 失敗しうる処理では `unwrap` / `expect` を避け、`?` と `map_err` で文脈を追加します（テストコードは除く）。
- 新しい依存は本当に必要か確認し、`Cargo.toml` と `Cargo.lock` をセットで更新してください。
- 公開範囲は最小にし、既存のモジュール分割・命名に合わせてください（`pub(crate)` を優先）。

## テスト

- 単体テストは各ファイル末尾の `#[cfg(test)] mod tests` に置きます。`tests/` にはフィクスチャと map transformation 用の JSON があります。
- ネットワークに依存するテストを追加しないでください。外部データが必要なテストは、ローカルのフィクスチャを使うか「データがなければ skip」の形にします。
- 一時ファイルは `tempfile` を使い、テスト後に残さないでください。
- グローバル状態（world floor、ブロックパレットなど）を変更するテストは、既存のロックや直列化の仕組みに従ってください。
- J-EGG500 の実データテストは `jegg500/` がある場合だけ実行されます。CI で skip されるのは正常です。
- 変更した層に応じて、座標変換、world editor の出力、ロケールキーなどのテストを追加してください。
- 仕上げに `cargo fmt -- --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test --all-targets --all-features` を通してください。GUI のシステム依存がない環境では、少なくとも `cargo test --no-default-features` を実行してください。

## Git / コミット

- **コミットメッセージは日本語で書いてください。**
- 件名は変更内容を一言で表し、本文に「なぜ」「影響範囲」「確認したテスト」を書きます。
- 1 コミット 1 トピックにし、無関係な変更を混ぜないでください。
- ユーザーから明示的に依頼されない限り、コミット・push・PR 作成はしないでください。
- 作業前に `git status --short` を確認し、既存の未コミット変更を上書きしないでください。
- このブランチと `main` の差分には J-EGG500 以外の変更も含まれます。差分を確認してから作業してください。

## 完了チェックリスト

- [ ] 変更内容に見合うテストを追加・更新した
- [ ] `cargo fmt -- --check` が通る
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` が通る
- [ ] `cargo test --all-targets --all-features`（または `--no-default-features`）が通る
- [ ] ユーザー向けの変更なら README / GUI ロケールを更新した
- [ ] `jegg500/` や生成物を追加していない
- [ ] コミットメッセージは日本語になっている
