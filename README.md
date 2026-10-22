<p align="center">
  <img src="assets/git/banner.png" alt="Arnis" width="747">
</p>

# Arnis

Arnis は、OpenStreetMap の地理データと標高データから、実在する街並みを Minecraft のワールドとして生成するツールです。Java Edition / Bedrock Edition / Luanti（Minetest）に対応しています。

このリポジトリは **`feature/jegg500-bathymetry`** ブランチです。日本周辺の海域に J-EGG500 の海底地形データを反映するため、次の機能を追加しています。

- J-EGG500（500 m メッシュ）の海底地形読み込み
- セル間の双線形補間（ON/OFF 切り替え可能）と、欠損データ時のフォールバック
- 回転したワールドでも、J-EGG500 をワールドに合わせて回転して適用
- GUI マップ上に J-EGG500 のタイル範囲を表示
- GUI / CLI から J-EGG500 の補間・J-EGG500 本体・従来の水深生成を個別に ON/OFF

## 主な機能

- OpenStreetMap の建物・道路・土地利用などを反映したワールド生成
- 標高データを使った地形生成
- 日本周辺の J-EGG500 水深データを反映する海底地形生成
- Java Edition / Bedrock Edition / Luanti（Minetest）向けの出力
- デスクトップ GUI と CLI の両方から実行可能

## 必要な環境

- [Rust](https://www.rust-lang.org/tools/install)（stable toolchain）
- GUI をビルドする場合は、各 OS の Tauri / WebKitGTK 依存パッケージ

CLI のみを使う場合は、GUI 用の依存パッケージは不要です。

### Linux

Tauri / WebKitGTK ベースの GUI をビルドするため、以下のようなパッケージが必要です。ディストリビューションによってパッケージ名は異なります。

- `pkg-config`
- OpenSSL 開発パッケージ（`libssl-dev` / `openssl-devel`）
- `libgtk-3-dev`
- `libwebkit2gtk-4.1-dev`
- `libjavascriptcoregtk-4.1-dev`
- `libsoup-3.0-dev`
- `libglib2.0-dev`
- `libcairo2-dev`
- `libpango1.0-dev`
- `libgdk-pixbuf-2.0-dev`
- `libwayland-dev`

Debian / Ubuntu 系では、例えば次のようにインストールできます。

```bash
sudo apt install \
  pkg-config libssl-dev libgtk-3-dev libwebkit2gtk-4.1-dev \
  libjavascriptcoregtk-4.1-dev libsoup-3.0-dev libglib2.0-dev \
  libcairo2-dev libpango1.0-dev libgdk-pixbuf-2.0-dev libwayland-dev
```

Nix を使う場合は `flake.nix` にこれらの依存関係が定義されています。

### Windows

- [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/) または Visual Studio（C++ 用ワークロード）
- Rust の MSVC toolchain

### macOS

- Xcode Command Line Tools

## セットアップ

```bash
# リポジトリをクローンしてブランチを切り替え
git clone https://github.com/wicon26-toyota/arnis.git
cd arnis
git switch feature/jegg500-bathymetry

# 依存関係を取得
cargo fetch
```

## ビルドと実行

### GUI 版

```bash
cargo run --release
```

Tauri ベースのデスクトップ GUI が起動します。マップ上で範囲を選択し、出力先のワールドフォルダを指定して **Start Generation** を押してください。

![Arnis GUI](assets/git/gui.png)

### CLI 版

GUI 機能を無効にして、軽量な CLI バイナリをビルドします。

```bash
cargo run --release --no-default-features -- \
  --output-dir="$HOME/.minecraft/saves/tokyo-bay" \
  --bbox="35.55,139.65,35.75,139.90"
```

`--bbox` は生成する範囲を `南西緯度,南西経度,北東緯度,北東経度` で指定します。

Windows の場合は、出力先を例えば次のように指定します。

```text
--output-dir="C:/Users/USER/AppData/Roaming/.minecraft/saves/worldname"
```

### 主な CLI オプション

| オプション | 説明 |
| --- | --- |
| `--output-dir` | 生成したワールドを出力するディレクトリ |
| `--bbox` | 生成範囲（`min_lat,min_lng,max_lat,max_lng`） |
| `--mode` | `geo-terrain`（標高＋OSM、デフォルト） / `geo-only`（平坦＋OSM） / `terrain-only`（標高のみ） |
| `--scale` | ワールドスケール（1.0 = 実寸） |
| `--body` | `earth` / `moon` / `mars`（月・火星は地形のみ） |
| `--rotation` | ワールドの回転角度（度、`-90`〜`90`、正の値で時計回り） |
| `--bedrock` | Bedrock Edition 用 `.mcworld` を生成 |
| `--luanti` | Luanti（Minetest）用 `map.sqlite` を生成 |
| `--jegg500-dir` | J-EGG500 データディレクトリを指定 |
| `--no-jegg500-bathymetry` | J-EGG500 を無効化（`--jegg500-dir` は無視） |
| `--no-jegg500-interpolation` | J-EGG500 の双線形補間を無効化し、最近傍セルの値を使う |
| `--no-legacy-water-depth` | 従来の海岸線距離ベースの水深生成を無効化 |
| `--help` | すべてのオプションを表示 |

### Nix で実行

```bash
nix run . -- \
  --output-dir="$HOME/.minecraft/saves/worldname" \
  --bbox="min_lat,min_lng,max_lat,max_lng"
```

## J-EGG500 海底地形データの利用

日本周辺の海域で J-EGG500 の水深データを反映するには、J-EGG500（500 m メッシュの海底地形データ）をダウンロードして配置します。

> **データの取り扱いについて**
>
> J-EGG500 のデータはこのリポジトリに含まれていません。利用条件については、JODC の「海洋データ・情報の使用上の注意」を確認してください。
>
> <https://www.jodc.go.jp/jodcweb/JDOSS/caution_j.html>
>
> ダウンロードしたデータをこのリポジトリへコミットしたり、第三者へ再配布したりしないでください。`jegg500/` ディレクトリは `.gitignore` に登録されており、Git の管理対象外になっています。
>
> J-EGG500 を利用した成果物を公開する場合は、「日本海洋データセンター」の資料を使用した旨を明記し、成果物1部を JODC に提出してください。

### 1. データをダウンロードする

海洋情報部（J-EGG500）のページから、必要なタイルをダウンロードしてください。

<https://www.jodc.go.jp/vpage/depth500_file_j.html>

ファイル名は `mesh500_<lat>_<lon>.txt` の形式です。緯度・経度は整数度のタイル単位になっています。

### 2. データを配置する

リポジトリ内の `jegg500/` ディレクトリ、または任意のディレクトリに、ダウンロードした `mesh500_*.txt` ファイルを配置します。

```text
jegg500/
├── mesh500_34_136.txt
├── mesh500_34_137.txt
├── mesh500_35_136.txt
└── ...
```

生成範囲に含まれるタイルが自動的に読み込まれます。デフォルトでは 500 m メッシュのセル間が双線形補間されるため、セル境界の段差を抑えた海底地形になります。欠損セルは補間から除外され、利用可能な近傍データから補間されます。補間をオフにすると、各ブロックに最も近いセルの値がそのまま使われるため、500 m の段差は残りますが、データ間にない水深を作りません。

### 3. 生成時に有効化する

#### GUI の場合

Settings の **水** セクションで、次の 3 つを切り替えられます。どれも初期状態はオンです。

- **J-EGG500 海底地形**: **J-EGG500 データフォルダ** に置いた J-EGG500 データを使うかどうか。オフにしてもフォルダの設定は保持されます。
- **海底地形の補間（バイリニア）**: 500 m メッシュのセル間を双線形補間して滑らかにするかどうか。オフにすると最近傍セルの値を使います。
- **従来の水深生成**: 海岸線からの距離で水深を推定する従来方式を使うかどうか。J-EGG500 のデータが欠けている海域のフォールバックとして使われます。

**J-EGG500 データフォルダ** で、データを配置したディレクトリを選択してください。空欄の場合は J-EGG500 を使用しません。フォルダを選択すると、マップ上にそのフォルダに存在する J-EGG500 の 1°タイル範囲が緑色の破線で表示されます。表示されるのはファイルが存在する範囲で、海底データの有無そのものではありません。

#### CLI の場合

`--jegg500-dir` でディレクトリを指定します。

```bash
cargo run --release --no-default-features -- \
  --output-dir="$HOME/.minecraft/saves/worldname" \
  --bbox="34.0,136.0,35.0,137.0" \
  --jegg500-dir="./jegg500"
```

従来の水深生成だけを無効にしたい場合は `--no-legacy-water-depth`、J-EGG500 だけを無効にしたい場合は `--no-jegg500-bathymetry` を指定します。双線形補間をオフにして最近傍セルの値を使う場合は `--no-jegg500-interpolation` を追加します。

### 4. 水深生成の組み合わせ

| J-EGG500 海底地形 | 従来の水深生成 | 結果 |
| --- | --- | --- |
| ON | ON | J-EGG500 がある海域は J-EGG500 の水深データ。欠損している海域は従来の推定水深で補完 |
| ON | OFF | J-EGG500 がある海域だけ J-EGG500 の水深データ。それ以外の海域はフラットな水 |
| OFF | ON | すべての海域で従来の推定水深 |
| OFF | OFF | すべての海域でフラットな水 |

補足:

- 「フラットな水」は、水深を掘らずに水面だけを生成する状態です。
- 回転（`--rotation` / GUI の回転角度）を指定した場合も、J-EGG500 の海底地形はワールドの回転に合わせて適用されます。回転後の範囲外やデータ欠損部では、従来の水深生成がオンならそちらで補完され、オフならフラットな水になります。

## ライセンス

Copyright (c) 2022-2026 Louis Erbkamm (louis-e)

Apache License 2.0 の下で配布されています。詳細は [LICENSE](LICENSE) を参照してください。

J-EGG500 のデータは日本海洋データセンター（JODC）から提供されています。本プロジェクトは J-EGG500 の生データを同梱・再配布しません。J-EGG500 を利用して作成した成果物を公開する場合は、出典として「日本海洋データセンター」の資料を使用した旨を明記し、成果物1部を JODC に提出してください。詳細は JODC の「海洋データ・情報の使用上の注意」を参照してください。

The Luanti block-name mapping in `src/luanti_block_map.rs` is derived from [MC2MT](https://github.com/rollerozxa/MC2MT) by rollerozxa and is licensed under the GNU Lesser General Public License v2.1 or later. The full attribution and license header are preserved in that file.

Download Arnis only from the official source <https://arnismc.com> or <https://github.com/louis-e/arnis/>. Every other website providing a download and claiming to be affiliated with the project is unofficial and may be malicious.

The logo was made by @nxfx21.

NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT.
