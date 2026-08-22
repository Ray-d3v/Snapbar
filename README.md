# Snapbar

Microsoft Teamsで他の参加者が共有している画面の**共有コンテンツ部分だけ**を、Teams会議画面に追従する操作バーから1クリックでWindowsクリップボードへコピーする軽量アプリです。

## ダウンロード

Windows向けのインストーラーとポータブル版は、[GitHub Releases](https://github.com/Ray-d3v/Snapbar/releases)で配布します。

- `Snapbar-Setup-x64.exe`: ユーザー単位インストーラー
- `Snapbar-portable-x64.exe`: インストール不要の実行ファイル

現在のベータは、上下・左右の参加者表示を除外する共有面検出、余白を確保した外枠なしピルUI、任意のPNG保存に対応しています。共有されているWindowsデスクトップのタスクバーは共有コンテンツの一部として残します。

現在はコード署名していないため、Windows SmartScreenの警告が表示される場合があります。

## 動作方針

- 必要なときだけ手動で起動し、閉じるとプロセスも終了します。
- 撮影結果は常にWindowsクリップボードへコピーします。
- 設定を有効にした場合だけ、Windowsで設定されている「スクリーンショット」フォルダーへPNGも保存します。
- アップロード、テレメトリ、バックグラウンドサービスはありません。
- 成功時にコピーするのは検出済みの共有コンテンツ領域だけです。Teamsウィンドウ全体へフォールバックしません。
- 共有領域を十分な確度で特定できない場合は、誤った範囲をコピーせずエラーにします。

## 操作バー

操作バーは外枠のない不透明な黒いピルです。状態、撮影、メニューの各操作が密着しないよう、横幅・内側余白・操作間隔を確保しています。

- 選択中のTeams会議画面の上部中央へ自動配置し、移動・サイズ変更・別モニターへの移動を追従します。
- Teams会議画面を最小化すると操作バーも非表示になり、復元すると再表示します。
- 左側にはTeamsの接続状態を表示します。複数のTeams候補がある場合はクリックで切り替えます。
- 中央の赤いカメラボタンで撮影します。
- 右端の三本線メニューアイコンへマウスを乗せるかクリックすると設定メニューが開きます。クリックした場合は開いた状態で固定できます。

設定メニューには次の操作があります。

- **ファイルにも保存**: Windowsの「スクリーンショット」フォルダーへPNGも保存するトグル。初期値はオフです。
- **対象を再検出**: Teams会議画面と共有コンテンツ範囲を再検出します。
- **Snapbarを終了**: アプリを完全終了します。

保存トグルは`%LOCALAPPDATA%\Snapbar\settings.conf`へ保持します。

## 共有コンテンツ領域の自動検出

固定ピクセルや固定割合では切り抜きません。Snapbarの起動時、対象変更時、Teamsのレイアウト変更時に次の順で領域を判定します。

1. Windows UI Automationから、Teams内の「共有画面」「shared content」「presentation」などに対応するUI要素と画面座標を取得します。
2. UI Automationの画面座標を、その時点の実キャプチャ画像サイズへ整数比で変換します。DPI、拡大率、負座標を含むマルチモニター、ウィンドウサイズの違いを吸収します。
3. 「参加者」「participants」「people」「chat」「toolbar」などのUI要素は除外領域として別管理します。
4. UI Automationが広い会議ステージしか返さない場合は、画像外周の色分布からTeams背景を推定し、共有面の行・列方向の連続密度を解析します。上側の参加者ストリップだけでなく、大きなウィンドウで右側に離れて表示される参加者列も共有面から分離します。
5. 縮小解析で得た共有面の四辺を元解像度の境界へ再スナップし、ウィンドウサイズやモニター解像度に依存しない座標を確定します。
6. 共有面下部にWindowsタスクバー特有の帯とアイコン構造がある場合は、その帯を共有デスクトップの一部として保持します。均一なTeams背景やレターボックスだけを除去します。
7. 特定した領域をキャッシュし、Windows Graphics Captureから届く最新フレームを共有領域だけに切り抜いて保持します。

赤い撮影ボタンを押した時点では、通常はUI Automation探索やフル画像解析をやり直しません。保持済みの最新フレームをクリップボードへコピーするため、連続撮影時の待ち時間を抑えます。

以下を検出した場合だけ、バックグラウンドで共有領域を再判定します。

- Teamsウィンドウのサイズまたはキャプチャ解像度の変更
- 別モニターへの移動やDPI変更
- チャット、参加者一覧などによるレイアウト変更
- キャッシュ済み領域の失効

Windows Graphics Captureの境界線は無効化し、Snapbar自身もキャプチャ対象から除外します。

## 操作

1. Teamsで会議または共有画面を表示します。ポップアウト表示にも対応します。
2. `Snapbar.exe`を起動します。
3. 操作バーが対象のTeams会議画面上部へ移動したことを確認します。
4. 必要に応じて右端のメニューからファイル保存を有効にします。
5. 中央の赤いカメラボタンを押します。
6. 成功すると共有コンテンツ範囲が明るくフラッシュし、画像がクリップボードへ入ります。保存トグルがオンならPNGも保存されます。
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
- Teamsが共有領域をUI Automationへ公開せず、画像上にも共有面の境界がない場合は安全に特定できないため撮影を中止します。
- TeamsのUI構造が大幅に変更された場合は検出規則の追従が必要になる可能性があります。その場合もウィンドウ全体はコピーしません。
- ZoomやGoogle Meetは現時点では自動検出対象外です。
- クリップボードに保持するのは最新の1枚です。アプリ内履歴はありません。

## 構成

```text
Teams-following GPUI control bar
    └─ persistent Windows Graphics Capture session
        ├─ UI Automation semantic detection
        ├─ participant/chat/chrome exclusion regions
        ├─ perimeter-background and row/column shared-surface detection
        └─ latest cropped frame cache
            ├─ Windows clipboard
            ├─ optional Windows Screenshots PNG
            └─ click-through flash overlay
```
