# Snapbar

Microsoft Teamsで他の参加者が共有している画面を、フローティングバーから1クリックでWindowsクリップボードへコピーする軽量アプリです。

![status: initial MVP](https://img.shields.io/badge/status-initial%20MVP-111111)

## 方針

- 必要なときだけ手動で起動
- 起動中は何度でも撮影可能
- 撮影ボタン1回で最新フレームをクリップボードへコピー
- 画像ファイルは自動保存しない
- ネットワーク通信、アップロード、テレメトリなし
- ウィンドウを閉じるとプロセスも終了
- Rust + GPUIによる小型の不透明ブラックUI

## 初期MVPの操作

1. Teamsの共有画面を可能なら「新しいウィンドウで開く」でポップアウトします。
2. `Snapbar.exe`を起動します。
3. 左端のウィンドウアイコンを押すと、Teams候補を再検出しながら次の候補へ切り替えます。
4. クロップアイコンで、ウィンドウ全体とTeams向けコンテンツプリセットを切り替えます。
5. 右端の赤い撮影ボタンを押します。成功するとボタンがチェック表示になり、画像がクリップボードへ入ります。
6. PowerPoint、OneNote、チャットなどへ貼り付けます。再度撮影するとクリップボードの画像は最新の1枚に更新されます。

ロックアイコンはバーの位置固定、`…`は対象情報・再検出・終了メニューです。

## 対象ウィンドウの選択

Teamsのプロセス名とウィンドウタイトルを基に候補を抽出し、共有・会議・プレゼンテーションを示す語、ウィンドウの前後関係、サイズなどから優先順位を付けます。誤ったTeamsウィンドウが選ばれた場合は、左端のアイコンを繰り返し押して候補を切り替えてください。

## クロップについて

初期MVPのコンテンツクロップは、Teamsポップアウトのタイトル領域を抑えるための保守的な比率プリセットです。Teamsのレイアウト、DPI、会議表示によっては完全一致しません。正確な手動範囲選択は次の実装段階です。切り落としを避けたい場合はクロップをオフにしてください。

## ビルド

Windows 11とRust stableを使用します。

```powershell
git clone https://github.com/Ray-d3v/Snapbar.git
cd Snapbar
cargo build --release
.\target\release\snapbar.exe
```

GitHub Actionsの`CI`ワークフローもWindows向けrelease exeをartifactとして生成します。

## 開発時チェック

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

## 既知の制約

- Windows 11専用です。
- Teams Premium等で画面キャプチャが禁止されている会議では、黒い画像になるなどOS・Teams側の制限を受けます。制限の回避は行いません。
- Teams以外のZoomやGoogle Meetは現時点では自動検出対象外です。
- クリップボードに保持するのは最新の1枚です。アプリ内履歴はありません。
- 現段階では実機Teams会議を用いた最終調整が必要です。

## 構成

```text
GPUI floating bar
    └─ capture request
        └─ xcap / Windows.Graphics.Capture
            └─ optional conservative crop
                └─ arboard / Windows clipboard
```
