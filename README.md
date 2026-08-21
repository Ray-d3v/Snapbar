# Snapbar

Microsoft Teamsで他の参加者が共有している画面の**共有コンテンツ部分だけ**を、Teams会議画面に追従する操作バーから1クリックでWindowsクリップボードへコピーする軽量アプリです。

## ダウンロード

Windows向けのインストーラーとポータブル版は、[GitHub Releases](https://github.com/Ray-d3v/Snapbar/releases)で配布します。

- `Snapbar-Setup-x64.exe`: ユーザー単位インストーラー
- `Snapbar-portable-x64.exe`: インストール不要の実行ファイル

現在はコード署名していないため、Windows SmartScreenの警告が表示される場合があります。

## 動作方針

- 必要なときだけ手動で起動し、閉じるとプロセスも終了します。
- 撮影結果はWindowsクリップボードだけへコピーします。自動保存、アップロード、テレメトリ、常駐処理はありません。
- 成功時にコピーするのは検出済みの共有コンテンツ領域だけです。Teamsウィンドウ全体へフォールバックしません。
- 共有領域を十分な確度で特定できない場合は、誤った範囲をコピーせずエラーにします。

## 操作バー

- 選択中のTeams会議画面の上部中央へ自動配置し、会議画面の移動・サイズ変更・別モニターへの移動を追従します。
- Teams会議画面を最小化すると操作バーも非表示になり、復元すると再表示します。
- 左の対象ボタンは、検出状態の表示と、複数あるTeams候補の切り替えに使います。
- 中央の赤い`スクショ`ボタンで撮影します。状態は`撮影中`、`コピー済み`、`再検出`、`再試行`として文字で表示します。
- 右の`…`から対象の再検出とアプリの終了を行います。

音声入力を示す波形表示や、用途の不明確なクロップ・位置固定ボタンはありません。

## 共有コンテンツ領域の自動検出

固定ピクセルや固定割合では切り抜きません。Snapbarの起動時、対象変更時、Teamsのレイアウト変更時に次の順で領域を判定します。

1. Windows UI Automationから、Teams内の「共有画面」「shared content」「presentation」などに対応するUI要素と画面座標を取得します。
2. UI Automationの画面座標を、その時点の実キャプチャ画像サイズへ整数比で変換します。DPI、拡大率、負座標を含むマルチモニター、ウィンドウサイズの違いを吸収します。
3. UI要素の意味、面積、中央寄りか、複数サンプル点から同じ領域が得られたかを評価します。
4. 「参加者」「participants」「people」「chat」「toolbar」などのUI要素は除外領域として別管理し、共有ステージ候補の端に含まれている場合は撮影範囲から取り除きます。複数の参加者タイルが縦に並ぶ場合も1つのサイドパネルとして扱います。
5. UI Automationだけで決まらない場合に限り、画像上の大きな内側矩形を解像度非依存で検出します。近接する参加者パネルを共有領域へ結合しないよう、ノイズ除去時に領域同士を膨張結合しません。
6. 共有映像の上下左右にTeams背景と同色のレターボックスがある場合は、その連続帯だけを追加で除去します。
7. 特定した領域をキャッシュし、Windows Graphics Captureから届く最新フレームを共有領域だけに切り抜いて保持します。

赤い撮影ボタンを押した時点では、通常はUI Automation探索やフル画像解析をやり直しません。保持済みの最新フレームをクリップボードへコピーするため、連続撮影時の待ち時間を抑えます。

以下を検出した場合だけ、バックグラウンドで共有領域を再判定します。

- Teamsウィンドウのサイズまたはキャプチャ解像度の変更
- 別モニターへの移動やDPI変更
- チャット、参加者一覧などによるレイアウト変更
- キャッシュ済み領域の失効

どの経路でも、ウィンドウ全体をコピーする動作はありません。

## 操作

1. Teamsで会議または共有画面を表示します。ポップアウト表示にも対応します。
2. `Snapbar.exe`を起動します。
3. 操作バーが対象のTeams会議画面上部へ移動したことを確認します。
4. 複数のTeams画面がある場合は、左の対象ボタンで切り替えます。
5. 中央の赤い`スクショ`ボタンを押します。
6. 成功すると共有コンテンツ範囲が約200ms明るくフラッシュし、画像がクリップボードへ入ります。
7. PowerPoint、OneNote、チャットなどへ貼り付けます。

## ビルド

Windows 11とRust stableを使用します。

```powershell
git clone https://github.com/Ray-d3v/Snapbar.git
cd Snapbar
cargo build --release
.\target\release\snapbar.exe
```

`main`へのpush時にGitHub Actionsが以下を実行します。

1. Format、Clippy、テスト
2. Windows releaseビルド
3. Inno Setupによるインストーラー生成
4. portable exeとインストーラーのartifact保存
5. GitHub prereleaseへの両ファイルの添付

## 開発時チェック

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

## 制約

- Windows 11専用です。
- Teams Premium等でキャプチャが禁止されている会議では、OS・Teams側の制限を受けます。制限の回避は行いません。
- Teamsが共有領域をUI Automationへ公開しない状態で、画像上にも領域境界がない場合は安全に特定できないため撮影を中止します。
- TeamsのUI構造が大幅に変更された場合は検出語や評価規則の追従が必要になる可能性があります。その場合もウィンドウ全体はコピーしません。
- ZoomやGoogle Meetは現時点では自動検出対象外です。
- クリップボードに保持するのは最新の1枚です。アプリ内履歴はありません。

## 構成

```text
Teams-following GPUI control bar
    └─ persistent Windows Graphics Capture session
        ├─ UI Automation semantic detection
        ├─ participant/chat/chrome exclusion regions
        ├─ conservative visual fallback
        └─ latest cropped frame cache
            ├─ Windows clipboard
            └─ click-through flash overlay
```
