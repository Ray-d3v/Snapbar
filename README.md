# Snapbar

Microsoft Teamsで他の参加者が共有している画面の**共有コンテンツ部分だけ**を、会議画面に追従する操作バーから1クリックでWindowsクリップボードへコピーする軽量Windowsアプリです。

## ダウンロード

Windows向けのインストーラーとポータブル版は、[GitHub Releases](https://github.com/Ray-d3v/Snapbar/releases)で配布します。

- `Snapbar-Setup-x64.exe`: ユーザー単位インストーラー
- `Snapbar-portable-x64.exe`: インストール不要の実行ファイル

現在はコード署名していないため、Windows SmartScreenの警告が表示される場合があります。

## 動作方針

- Snapbarを手動で起動すると、通知領域に常駐してTeams会議をローカル監視します。
- Windowsログオン時の自動起動は現時点では登録しません。
- 会議へ参加していない間は操作バーを表示しません。
- 会議参加を複合条件で確認すると、対象Teamsウィンドウの上部中央へ操作バーを自動表示します。
- Teamsを最小化した場合は退出扱いにせず、操作バーだけを非表示にします。Teamsを復元すると再表示します。
- 会議退出を一定時間確認すると操作バーとキャプチャセッションを停止し、通知領域で次の会議を待ちます。
- アプリを完全終了するには、操作バーのメニューまたは通知領域のSnapbarアイコンを右クリックして`Snapbarを終了`を選択します。
- アップロード、テレメトリ、クラウド認証、Windowsサービスはありません。

## 会議の自動検出

外部EXE向けのTeams参加・退出イベントには依存せず、この端末上のTeams UIを監視します。

1. `SetWinEventHook`でウィンドウの生成、表示、フォーカス、最小化、移動、サイズ変更を受信します。
2. イベント取りこぼし対策として約700ms間隔のウォッチドッグ確認も行います。
3. Teams候補ごとに以下のローカル信号を確認します。
   - Teamsプロセス／Teams会議ウィンドウ
   - `TeamsWebView`子ウィンドウ
   - `TeamsVideo`子ウィンドウ
   - Windows UI Automation上の`退出`、`通話を終了`、`Leave call`などの操作
   - UI Automation上の共有コンテンツ要素
4. 単一の信号だけでは会議参加と判定しません。複数信号が同じウィンドウで2回以上、概ね0.6〜1.1秒安定した場合に参加確定とします。
5. 退出は、会議信号の消失を2回以上かつ約1.4秒継続して確認してから確定します。
6. 最小化中は会議状態を維持します。

`TeamsVideo`やTeamsのUIA構造は公開契約ではないため、特定の1要素だけに依存せず、WinEvent、子ウィンドウ、UIA、継続時間を組み合わせています。

## 共有コンテンツ領域の確定

共有画面の切り抜きは画像内容やテーマ色から推測しません。TeamsがWindows UI Automationへ公開する共有コンテンツ要素を正本にします。

1. 選択中のTeams HWNDからUI Automationルートを取得します。
2. UIAサブツリー全体を走査し、`IsOffscreen == false`で、Accessible Nameに「共有」と「コンテンツ」の両方を含む要素を探します。英語環境では`shared content`、`presented content`などを使用します。
3. 実機で確認された`ControlType.MenuItem`を最優先し、`Document`、`Pane`、`Custom`、`Group`、`Image`も互換候補として扱います。
4. `AutomationId`や動的な`fui-*`クラス名には依存しません。
5. `BoundingRectangle`がTeamsウィンドウ内にほぼ完全に収まり、十分な大きさであることを確認します。
6. WebView2のUIAツリーが未展開の場合は、`ElementFromPoint`で一度ウォームアップし、短時間待って再走査します。
7. 同じ矩形を2回取得し、各辺が数ピクセル以内で安定した場合だけ確定します。
8. UIA画面座標を、その時点のWindows Graphics Capture画像座標へ変換します。DPI、負座標を含むマルチモニター、ウィンドウサイズの違いを吸収します。
9. 確定したUIA矩形には、色解析、三層境界、行・列投影などの画像補正を適用しません。

共有領域を一意に確定できない場合は、Teamsウィンドウ全体や画像推定範囲をコピーせず、失敗として停止します。

## 操作バーとホバーメニュー

操作バーは外枠のない不透明な黒いピルです。

- 対象Teamsウィンドウの移動、リサイズ、最大化、別モニターへの移動へ追従します。
- タスクバーとAlt+Tabには表示しません。
- Teamsからフォーカスを奪わずに表示します。
- Snapbar自身はキャプチャ画像から除外します。
- 共有コンテンツUIA要素と最新フレームが準備できるまで、撮影ボタンはグレー表示で無効です。
- 共有開始後に準備が完了すると、撮影ボタンが赤色になり有効化されます。
- 右端の三本線メニューには、PNG保存トグル、会議・共有の再検出、アプリ終了を配置しています。
- メニューは約110msのホバーで開き、カーソルが外れても約220msの猶予後に閉じます。猶予中に戻れば閉じません。
- メニューボタンをクリックすると固定表示になり、再クリックで閉じます。
- Win32の入力リージョンを黒いピルとメニューの形に合わせているため、透明な余白や隙間はTeamsへのクリックを遮りません。

通知領域のSnapbarアイコンを右クリックすると、以下を実行できます。

- **会議を再検出**
- **Snapbarを終了**

## スクリーンショット

- 撮影結果は必ずWindowsクリップボードへコピーします。
- `ファイルにも保存`を有効にした場合だけ、Windowsが設定しているScreenshots既知フォルダーへPNGを保存します。
- 保存トグルの初期値はオフです。
- 保存設定は`%LOCALAPPDATA%\Snapbar\settings.conf`へ保持します。
- 撮影成功後、実際に取得した共有領域へ短い白いフラッシュを表示します。
- 共有されているWindowsデスクトップのタスクバーは共有コンテンツに含まれます。
- 参加者映像、チャット、Teams操作UIはUIA共有領域の外側なので含まれません。

## メモリと処理負荷

- Windows Graphics Captureセッションは、会議中でも共有コンテンツが存在するときだけ開始します。共有終了・会議退出時には停止します。
- UIA座標の算出だけのためにTeamsウィンドウ全体をCPUメモリへコピーしません。
- CPU側へ取得するのは確定済みの共有領域だけです。
- 最新の切り抜き画像は1枚だけ保持し、同じ`Vec<u8>`を再利用します。フレームごとの大規模なヒープ確保を避けます。
- 待機時のバックアップ更新は約750ms間隔です。撮影ボタンを押したときだけ次の新しいフレームを最大約45ms待ち、来なければ直近のバックアップを使います。
- UIAの全走査は共有開始・レイアウト変更時と低頻度の整合性確認に限定します。
- 操作バーのTeams追従は位置が変化した場合だけ`SetWindowPos`を実行します。

GPUIとWindows Graphics Capture自体の基礎メモリは必要ですが、旧実装にあったフルフレーム二重コピー、20fpsのCPUキャッシュ、共有前のWGC起動を廃止しています。

## 操作

1. `Snapbar.exe`を起動します。操作バーは表示されず、通知領域で待機します。
2. Teams会議へ参加します。
3. 会議参加が確定すると、Teams会議画面上部へ操作バーが自動表示されます。
4. 他の参加者が画面共有を開始すると、共有領域の確定後に撮影ボタンが有効になります。
5. 赤いカメラボタンを押します。
6. PowerPoint、OneNote、Teamsチャットなどへ`Ctrl + V`で貼り付けます。
7. 会議退出後は操作バーが自動で消え、Snapbarは通知領域で次の会議を待ちます。

## ビルド

Windows 11とRust stableを使用します。

```powershell
git clone https://github.com/Ray-d3v/Snapbar.git
cd Snapbar
cargo build --release
.\target\release\snapbar.exe
```

`main`へのpush時にGitHub Actionsが以下を実行します。

1. Format、Clippy、単体テスト
2. Windows releaseビルド
3. Inno Setupによるインストーラー生成
4. ポータブル版とインストーラーのartifact保存
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
- Teamsのローカルウィンドウ構造とUI Automation構造は公開契約ではありません。Teams更新時に検出規則の追従が必要になる可能性があります。
- 共有コンテンツ要素をUI Automationから確定できない環境では、誤撮影を避けるため撮影ボタンを有効化しません。
- 音声のみの会議では`TeamsVideo`がない場合があります。そのため、退出操作など他の会議信号を含めて判定します。
- ZoomやGoogle Meetは現時点では対象外です。
- クリップボードに保持するのは最新の1枚です。アプリ内履歴はありません。

## 構成

```text
Resident Snapbar process
├─ notification-area controller
├─ SetWinEventHook + watchdog meeting monitor
│  ├─ TeamsWebView / TeamsVideo child-window evidence
│  ├─ UIA leave-call evidence
│  └─ debounced join / leave state
└─ hidden GPUI control bar
   └─ active meeting detected → follow Teams window and show
      ├─ debounced hover menu + visible-shape input region
      └─ shared content detected → start Windows Graphics Capture
         ├─ authoritative UIA shared-content BoundingRectangle
         ├─ cropped-only reusable CPU buffer
         └─ one low-frequency latest-frame backup
            ├─ Windows clipboard
            ├─ optional Windows Screenshots PNG
            └─ click-through flash overlay
```
