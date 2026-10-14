# Arnis

Arnis は、OpenStreetMap の地理データと標高データ、およびこのブランチでは J-EGG500 の海底地形データを組み合わせて、Minecraft Java Edition / Bedrock Edition、Luanti（Minetest）向けのワールドを生成するツールです。

このリポジトリは **`feature/jegg500-bathymetry`** ブランチです。日本周辺の海域で実測に基づく水深を反映する J-EGG500 対応が入っています。

## 必要な環境

- [Rust](https://www.rust-lang.org/tools/install)（stable toolchain）
- 各 OS のビルド依存関係

### Linux

Tauri / WebKitGTK ベースの GUI をビルドするため、以下のようなパッケージが必要です。ディストリビューションによってパッケージ名は異なります。

- `pkg-config`
- `libssl-dev` / `openssl-devel`
- `libgtk-3-dev`
- `libwebkit2gtk-4.1-dev`
- `libjavascriptcoregtk-4.1-dev`
- `libsoup-3.0-dev`
- `libglib2.0-dev`
- `libcairo2-dev`
- `libpango1.0-dev`
- `libgdk-pixbuf-2.0-dev`
- `libwayland-dev`

Nix を使う場合は `flake.nix` にこれらの依存関係が定義されています。

### Windows

- [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/) または Visual Studio（C++ 用ワークロード）
- Rust の MSVC toolchain

### macOS

- Xcode Command Line Tools

## セットアップ

```bash
# リポジトリをクローン
git clone https://github.com/louis-e/arnis.git
cd arnis

# 依存関係を取得
cargo fetch
```

## ビルドと実行

### GUI 版

```bash
cargo run --release
```

ブラウザベースの UI が起動します。マップ上で範囲を選択し、出力先のワールドフォルダを指定して **Start Generation** を押してください。

### CLI 版

GUI 機能を無効にして軽量な CLI バイナリをビルドします。

```bash
cargo run --release --no-default-features -- \
  --output-dir="C:/Users/USER/AppData/Roaming/.minecraft/saves/worldname" \
  --bbox="min_lat,min_lng,max_lat,max_lng"
```

`--bbox` は生成する範囲を `南西緯,南西経,北東緯,北東経` で指定します。例：

```bash
cargo run --release --no-default-features -- \
  --output-dir="$HOME/.minecraft/saves/tokyo-bay" \
  --bbox="35.55,139.65,35.75,139.90"
```

主要なオプション：

| オプション | 説明 |
| --- | --- |
| `--output-dir` | 生成したワールドを出力するディレクトリ |
| `--bbox` | 生成範囲（`min_lat,min_lng,max_lat,max_lng`） |
| `--mode` | `geo-terrain`（標高＋OSM、デフォルト） / `geo-only`（平坦＋OSM） / `terrain-only`（標高のみ） |
| `--scale` | ワールドスケール（1.0 = 実寸） |
| `--bedrock` | Bedrock Edition 用 `.mcworld` を生成 |
| `--luanti` | Luanti（Minetest）用 `map.sqlite` を生成 |
| `--jegg500-dir` | J-EGG500 データディレクトリを指定 |

### Nix で実行

```bash
nix run github:louis-e/arnis -- \
  --output-dir="$HOME/.minecraft/saves/worldname" \
  --bbox="min_lat,min_lng,max_lat,max_lng"
```

## J-EGG500 海底地形データの利用

このブランチでは、日本周辺の海域で実測水深を反映できるよう、J-EGG500（500 m メッシュの海底地形データ）に対応しています。

### 1. データをダウンロードする

海洋情報部（J-EGG500）のページから、必要なタイルをダウンロードしてください。

[https://www.jodc.go.jp/vpage/depth500_file_j.html](https://www.jodc.go.jp/vpage/depth500_file_j.html)

ファイル名は `mesh500_<lat>_<lon>.txt` の形式になっています。緯度・経度は整数度のタイル単位です。

### 2. データを配置する

リポジトリ内の `jegg500/` ディレクトリ、または任意のディレクトリにダウンロードした `mesh500_*.txt` ファイルを配置します。

例: 
```text
jegg500/
├── mesh500_34_136.txt
├── mesh500_34_137.txt
├── mesh500_35_136.txt
└── ...
```

生成範囲に含まれるタイルが自動的に読み込まれます。一部のタイルが欠けていても、データがない海域は従来の海岸線距離に基づく水深でフォールバックします。

### 3. 生成時にディレクトリを指定する

#### GUI の場合

Settings の **J-EGG500 海底地形** 欄で、データを配置したディレクトリを選択してください。空欄の場合は J-EGG500 を使用せず、従来の shoreline-distance ベースの水深になります。

#### CLI の場合

`--jegg500-dir` でディレクトリを指定します。

```bash
cargo run --release --no-default-features -- \
  --output-dir="$HOME/.minecraft/saves/worldname" \
  --bbox="34.0,136.0,35.0,137.0" \
  --jegg500-dir="./jegg500"
```

Wrote by AI
