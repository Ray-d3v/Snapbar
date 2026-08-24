# Snapbar

Microsoft Teamsで他の参加者が共有している画面の**共有コンテンツ部分だけ**を、Teams会議画面に追従する操作バーから1クリックでWindowsクリップボードへコピーする軽量アプリです。

## ダウンロード

Windows向けのインストーラーとポータブル版は、[GitHub Releases](https://github.com/Ray-d3v/Snapbar/releases)で配布します。

- `Snapbar-Setup-x64.exe`: ユーザー単位インストーラー
- `Snapbar-portable-x64.exe`: インストール不要の実行ファイル

現在のベータは、TeamsがWindows UI Automationへ公開する共有コンテンツ要素を正本として切り抜きます。共有内容の色やダークモードに依存せず、参加者映像、チャット、Teams操作UIを含めません。共有されているWindowsデスクトップのタスクバーは共有コンテンツの一部として残します。

現在はコード署名していないため、Windows SmartScreenの警告が表示される場合があります。

## 動作方針

- 必要なときだけ手動で起動し、閉じるとプロセスも終了します。
- 撮影結果は常にWindowsクリップボードへコピーします。
- 設定を有効にした場合だけ、Windowsで設定されている「スクリーンショット」フォルダーへPNGも保存します。
- アップロード、テレメトリ、バックグラウンドサービスはありません。
- 成功時にコピーするのは検出済みの共有コンテンツ領域だけです。Teamsウィンドウ全体へフォールバックしません。
- 共有領域を確定できない場合は、画像から推測した範囲を勝手に撮影せずエラーにします。

## 操作バー

操作バーは外枠のない不透明な黒いピルです。状態、撮影、メニューの各操作が密着しないよう、横幅・内側余白・操作間隔を確保しています。

- 選択中のTeams会議画面の上部中央へ自動配置し、移動・サイズ変更・別モニターへの移動を追従します。
- Teams会議画面を最小化すると操作バーも非表示になり、復元すると再表示します。
- 左側には中立色のウィンドウアイコンとTeams対象名を表示します。複数のTeams候補がある場合はクリックで切り替えます。
- 中央の赤いボタンは常に白いカメラアイコンを表示します。
- 右端の三本線メニューへマウスを乗せるかクリックすると設定メニューが開きます。クリックした場合は開いた状態で固定できます。

設定メニューには次の操作があります。

- **ファイルにも保存**: Windowsの「スクリーンショット」フォルダーへPNGも保存するトグル。初期値はオフです。
- **対象を再検出**: Teams会議画面と共有コンテンツ範囲を再検出します。
- **Snapbarを終了**: アプリを完全終了します。

保存トグルは`%LOCALAPPDATA%\Snapbar\settings.conf`へ保持します。

## 共有コンテンツ領域の自動検出

固定ピクセル、固定割合、共有画面の明暗、Teamsテーマ色による切り抜きは正本にしません。Snapbarの起動時、対象変更時、Teamsのレイアウト変更時に次の順で領域を確定します。

1. 選択中のTeams HWNDからWindows UI Automationのルート要素を取得します。
2. Teamsウィンドウ配下のUIAサブツリーを走査し、`IsOffscreen == false`で、Accessible Nameが「共有」と「コンテンツ」の両方を含む要素を探します。英語環境では`shared content`などの強い共有コンテンツ名を使用します。
3. 現在のTeams実機で確認された`ControlType.MenuItem`を最優先し、`Document`、`Pane`、`Custom`、`Group`、`Image`も互換候補として扱います。`AutomationId`や動的な`fui-*`クラス名には依存しません。
4. 要素の`BoundingRectangle`がTeamsウィンドウ内にほぼ完全に収まり、共有面として十分な大きさであることを検証します。
5. WebView2のUIAツリーが未展開で候補がない場合は、共有面中央付近を`ElementFromPoint`で一度ウォームアップし、短時間待ってからツリーを1回だけ再取得します。
6. 候補矩形を2回取得し、各辺が数ピクセル以内で安定している場合だけ確定します。最高優先度の候補が複数ある場合は自動選択しません。
7. UIAの画面座標を、その時点のWindows Graphics Capture画像へ整数比で変換します。負座標を含むマルチモニター、DPI、拡大率、ウィンドウサイズの違いを吸収します。
8. 確定したUIA矩形はそのまま使用し、色解析、三層境界、行・列密度などの画像ヒューリスティックで再加工しません。このため共有内容がダークモードでも、Teams背景と同系色でも判定が変わりません。
9. 特定した領域をキャッシュし、Windows Graphics Captureから届く最新フレームを共有領域だけに切り抜いて保持します。

画像解析コードは、UIA要素を取得できなかった場合に診断候補を作るためだけに残しています。診断候補は自動撮影には使いません。

赤い撮影ボタンを押した時点ではUI Automation探索をやり直しません。保持済みの最新フレームをクリップボードへコピーするため、連続撮影時の待ち時間を抑えます。

以下を検出した場合だけ、バックグラウンドで共有領域を再判定します。

- Teamsウィンドウのサイズまたはキャプチャ解像度の変更
- 別モニターへの移動やDPI変更
- チャット、参加者一覧などによるレイアウト変更
- 定期的なUIA整合性確認

定期確認時だけUIAが一時的に取得できなかった場合は、キャプチャサイズとレイアウトが変わっていないことを条件に、直前の確定済み矩形を継続使用します。サイズ変更やレイアウト変更後に再確定できない場合は、古い範囲を使わず撮影を停止します。

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
- Teamsが共有領域をUI Automationへ公開しない環境では、誤撮影防止のため自動撮影を中止します。画像推定だけで撮影範囲を確定しません。
- TeamsのAccessible NameやUIA構造は公開契約ではないため、Teams更新後に検出規則の追従が必要になる可能性があります。その場合もウィンドウ全体はコピーしません。
- ZoomやGoogle Meetは現時点では自動検出対象外です。
- クリップボードに保持するのは最新の1枚です。アプリ内履歴はありません。

## 構成

```text
Teams-following GPUI control bar
    └─ persistent Windows Graphics Capture session
        ├─ Teams HWND-scoped UI Automation subtree scan
        ├─ WebView2 ElementFromPoint warm-up and one retry
        ├─ unique, stable shared-content BoundingRectangle
        ├─ diagnostic-only image candidate generation
        └─ latest authoritative cropped frame cache
            ├─ Windows clipboard
            ├─ optional Windows Screenshots PNG
            └─ click-through flash overlay
```
