# Snapbar

Microsoft Teamsで他の参加者が共有している画面の**共有コンテンツ部分だけ**を、Teamsのタイトルバー中央から1クリックでWindowsクリップボードへコピーする軽量Windowsアプリです。

## ダウンロード

Windows向けのインストーラーとポータブル版は、[GitHub Releases](https://github.com/Ray-d3v/Snapbar/releases)で配布します。

- `Snapbar-Setup-x64.exe`: ユーザー単位インストーラー
- `Snapbar-portable-x64.exe`: インストール不要の実行ファイル

現在はコード署名していないため、Windows SmartScreenの警告が表示される場合があります。

## 動作方針

- Snapbarを手動で起動すると、通知領域に常駐してTeams会議をローカル監視します。
- Windowsログオン時の自動起動は現時点では登録しません。
- 会議へ参加していない間はタイトルバーの操作UIを表示しません。
- 会議参加を複合条件で確認すると、対象Teamsウィンドウのタイトルバー中央へ小さなSnapbarピルを自動表示します。
- Teamsを最小化した場合は退出扱いにせず、Snapbarだけを非表示にします。Teamsを復元すると再表示します。
- 会議退出を一定時間確認するとSnapbarとキャプチャセッションを停止し、通知領域で次の会議を待ちます。
- アプリを完全終了するには、展開したタイトルバー操作列の電源ボタン、または通知領域のSnapbarアイコンを右クリックして`Snapbarを終了`を選択します。
- アップロード、テレメトリ、クラウド認証、Windowsサービスはありません。

タイトルバーの表示パターンは起動引数で選べます。引数なしでは従来どおり、ホバーでDynamic Island風に展開するパターンです。`--inline-titlebar`（短縮形`--inline`）を付けると、ホバー時に操作列がタイトルバー内だけで横方向へ展開します。

GitHub Releasesのインストーラー版は、インストール完了時の起動、スタートメニュー、任意で作成するデスクトップショートカットのすべてに`--inline-titlebar`を設定します。そのため、通常のインストール操作ではタイトルバー内だけで完結する表示が既定になります。インストール先の`Snapbar.exe`を直接起動した場合と、ポータブル版を引数なしで起動した場合は従来の表示です。

Debugビルドでは、動きの確認や画面録画ができるようタイトルバーUIと撮影成功時の白いフラッシュをキャプチャ対象に含めます。Releaseビルドでも開発中に録画したい場合だけ`--recordable-overlay`を付けてください。通常のRelease起動では、従来どおりSnapbar自身とフラッシュを画面キャプチャから除外します。

## 会議の自動検出

外部EXE向けのTeams参加・退出イベントには依存せず、この端末上のTeams UIを監視します。

1. `SetWinEventHook`でウィンドウの生成、表示、フォーカス、最小化、移動、サイズ変更を受信します。
2. イベント取りこぼし対策として約700ms間隔のウォッチドッグ確認も行います。
3. Teams候補ごとに以下のローカル信号を確認します。
   - Teamsプロセス／Teams会議ウィンドウ
   - `TeamsWebView`子ウィンドウ
   - `TeamsVideo`子ウィンドウ
   - Windows UI Automation上の`退出`、`通話を終了`、`Leave call`などの操作
   - UI Automation上の通話時間／退出コントロールを示す安定した`AutomationId`
   - UI Automation上の共有コンテンツ要素
4. 単発の検出だけでは会議参加と判定しません。退出／通話時間などの会議固有信号が同じウィンドウで2回以上、概ね0.6〜1.1秒安定した場合に参加確定とします。
5. 退出は、会議信号の消失を2回以上かつ約1.4秒継続して確認してから確定します。
6. 最小化中は会議状態を維持します。

`TeamsVideo`やTeamsのUIA構造は公開契約ではないため、特定の1要素だけに依存せず、WinEvent、子ウィンドウ、UIA、継続時間を組み合わせています。

## 共有コンテンツ領域の確定

共有画面の切り抜きは画像内容やテーマ色から推測しません。TeamsがWindows UI Automationへ公開する共有コンテンツ要素を正本にします。

1. 選択中のTeams HWNDからWindows UI Automationルートを取得します。
2. UIAサブツリー全体を走査し、`IsOffscreen == false`で、Accessible Nameに「共有」と「コンテンツ」の両方を含む要素を探します。英語環境では`shared content`、`presented content`などを使用します。
3. 実機で確認された`ControlType.MenuItem`を最優先し、`Document`、`Pane`、`Custom`、`Group`、`Image`も互換候補として扱います。
4. `AutomationId`や動的な`fui-*`クラス名には依存しません。
5. `BoundingRectangle`がTeamsウィンドウ内にほぼ完全に収まり、十分な大きさであることを確認します。
6. WebView2のUIAツリーが未展開の場合は、`ElementFromPoint`で一度ウォームアップし、短時間待って再走査します。
7. 同じ矩形を2回取得し、各辺が数ピクセル以内で安定した場合だけ確定します。
8. UIA画面座標を、その時点のWindows Graphics Capture画像座標へ変換します。DPI、負座標を含むマルチモニター、ウィンドウサイズの違いを吸収します。
9. 確定したUIA矩形には、色解析、三層境界、行・列投影などの画像補正を適用しません。

共有領域を一意に確定できない場合は、Teamsウィンドウ全体や画像推定範囲をコピーせず、失敗として停止します。

## タイトルバー操作UI

Snapbarは会議コンテンツの上へ常時重ねず、Teamsの**タイトルバー中央の空き領域**だけを使用します。

### 通常時

- 約`92 × 30px`の領域を、最小化・最大化ボタンと同じ考え方の`46px`幅セル2個としてタイトルバー内へ収めます。
- 背景、影、ラベル、下方向のせり出しは表示せず、状態ドットと`16px`のカメラアイコンだけをタイトルバー上へ置きます。
- ホバー入力範囲はこの`92 × 30px`の矩形と一致し、Teamsタイトルバーの縦幅を上端から下端まで覆います。左右の領域はTeamsへ透過するため、ウィンドウのドラッグを妨げません。

### ホバー時

約16ms（60Hz表示の約1フレーム）ホバーすると、タイトルバーへ接続したまま中央を基準に、Dynamic Island風の操作列へ約`272 × 46px`まで展開します。展開面はTeamsタイトルバーの実表示色へ追従し、ライトテーマでは暗い文字と操作面、ダークテーマでは明るい文字と操作面へ自動反転します。タイトルバー下端から根本が四分円状に内側へカーブし、同色の面が約`16px`下へ膨らみます。形状と内容は約150〜200msで展開し、ピークでわずかに膨らんでから約300msまでに収束します。下へ別メニューは開かず、中央の安全領域からも動きません。

未ホバー時のカメラアイコンと展開後の赤い撮影ボタンは、同じX座標へ固定しています。ホバー後もカーソルを横へ追い直す必要はありません。

左から次の操作を配置しています。

- 状態表示／再検出
- Windowsのスクリーンショットフォルダーにも保存するトグル
- 共有画面を撮影してクリップボードへコピー
- 会議・共有の再検出
- Snapbarを終了

カーソルが操作列から外れて約50ms経過すると自動で縮みます。ホバー判定はGPUI要素ではなく、オーバーレイHWNDへ`SetWindowSubclass`で追加した単一のWin32状態機械が`TrackMouseEvent`／`WM_MOUSELEAVE`とウィンドウタイマーを処理します。タイマー発火時には`WindowFromPoint`でも実際のポインター位置を再確認し、展開中だけ低頻度の安全確認を行います。クリック固定と常時カーソルポーリングはありません。

### インライン時（`--inline-titlebar`）

- 会議UIが表示されると、92pxの待機セルが表示されます。そこへホバーすると、操作列が横方向へ展開します。
- 30pxのタイトルバー帯の上端1 logical pxをWindowsの外周線用に残し、表示面は最大`272 × 29 logical px`に収めます。下端をタイトルバー下端へ固定するため、展開しても外周線を隠さず、タイトルバー下側へもせり出しません。
- 展開は中央のカメラ位置を軸に、背景が左右へ約180msかけてニュッと広がります。30fpsの画面録画でも途中の5〜6フレームを追えるテンポで、操作ボタンも中央から左右へ連続的に散開し、最後にごく小さくバネで収まるDynamic Island風のアニメーションです。閉じている途中で再ホバーした場合も、現在の幅から逆向きの速度を引き継がずに開き直すため、中央のボタンだけで一度止まる二段階表示にはなりません。形状は変えず、ホバー判定のデバウンスも維持します。
- 状態、保存先、撮影、再検出、終了を1列に保ち、ポインターが実際の表示面から外れると既存のデバウンス後に92pxへ戻ります。会議終了・Teams最小化・安全領域の変化でも非表示になります。
- タイトルバーの安全幅が足りない場合は、同じ上端インセットを保った`46 × 29 logical px`のカメラセルへ縮退します。
- 既定のホバー展開パターンへ戻すには、起動引数なしで`Snapbar.exe`を起動します。

### 配置と狭いウィンドウ

- `DWMWA_EXTENDED_FRAME_BOUNDS`でTeamsの表示枠を取得します。
- `DWMWA_CAPTION_BUTTON_BOUNDS`で右側の最小化・最大化・閉じるボタン領域を避けます。
- 左側の会議名と右側のキャプションボタンを避けた中央領域へ配置します。
- Teamsの移動、最大化、スナップ、別モニターへの移動、DPI変更へ追従します。
- 横幅が足りない場合は、`46 × 30px`の透明なキャプションセル内にあるカメラアイコン1個だけへ縮退します。
- カメラボタンも安全なタイトルバー領域へ収まらない場合は、Teamsの操作を妨げないようSnapbarを非表示にします。
- タスクバーとAlt+Tabには表示せず、Teamsからフォーカスを奪いません。
- Snapbar自身はキャプチャ画像から除外します。

通知領域のSnapbarアイコンを右クリックすると、以下を実行できます。

- **会議を再検出**
- **Snapbarを終了**

## スクリーンショット

- 撮影結果は必ずWindowsクリップボードへコピーします。
- フォルダーアイコンを有効にした場合だけ、Windowsが設定しているScreenshots既知フォルダーへPNGを保存します。
- 保存トグルの初期値はオフです。
- 保存設定は`%LOCALAPPDATA%\Snapbar\settings.conf`へ保持します。
- 撮影成功後、実際に取得した共有領域へ短い白いフラッシュを表示します。Debugビルドまたは`--recordable-overlay`起動では、このフラッシュも画面録画へ映ります。
- 共有されているWindowsデスクトップのタスクバーは共有コンテンツに含まれます。
- 参加者映像、チャット、Teams操作UIはUIA共有領域の外側なので含まれません。

## メモリと処理負荷

- Windows Graphics Captureセッションは、会議中でも共有コンテンツが存在するときだけ開始します。共有終了・会議退出時には停止します。
- UIA座標の算出だけのためにTeamsウィンドウ全体をCPUメモリへコピーしません。
- CPU側へ取得するのは確定済みの共有領域だけです。
- 最新の切り抜き画像は1枚だけ保持し、同じ`Vec<u8>`を再利用します。フレームごとの大規模なヒープ確保を避けます。
- 待機時のバックアップ更新は約750ms間隔です。撮影ボタンを押したときだけ次の新しいフレームを最大約45ms待ち、来なければ直近のバックアップを使います。
- UIAの全走査は共有開始・レイアウト変更時と低頻度の整合性確認に限定します。
- タイトルバーUIのTeams追従は位置が変化した場合だけ`SetWindowPos`を実行します。
- ホバーアニメーション中はGPUIの非アクティブウィンドウ向け約30fps制限を使わず、DWMのvsyncへ追従します。実際の描画レートは、Snapbarを表示しているモニターの設定リフレッシュレートとWindowsの描画負荷が上限です。

GPUIとWindows Graphics Capture自体の基礎メモリは必要ですが、旧実装にあったフルフレーム二重コピー、20fpsのCPUキャッシュ、共有前のWGC起動を廃止しています。

## 操作

1. 既定のホバー展開を使う場合は`Snapbar.exe`、タイトルバー内だけのインライン表示を使う場合は`Snapbar.exe --inline-titlebar`を起動します。タイトルバーUIは表示されず、通知領域で待機します。
2. Teams会議へ参加します。
3. 会議参加が確定すると、Teamsタイトルバー中央へ小さなSnapbarアイランドが表示されます。
   - 参加者が自分だけで、まだ共有画面がない状態でも表示されます。撮影機能だけが共有開始まで待機します。
4. 既定モードではアイランドへ約16msホバーして操作列を表示します。インラインモードでも、同じ待ち時間のあとタイトルバー内へ操作列が横に展開します。
5. 他の参加者が画面共有を開始し、カメラボタンが赤くなったらクリックします。
6. PowerPoint、OneNote、Teamsチャットなどへ`Ctrl + V`で貼り付けます。
7. 会議退出後はタイトルバーUIが自動で消え、Snapbarは通知領域で次の会議を待ちます。

## ビルド

Windows 11とRust stableを使用します。

```powershell
git clone https://github.com/Ray-d3v/Snapbar.git
cd Snapbar
cargo build --release
.\target\release\snapbar.exe
```

インライン表示を画面録画へ含める開発用ランチャーは、Releaseビルド後に次で起動できます。

```powershell
.\target\release\snapbar_recordable_inline_launcher.exe
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
└─ hidden GPUI title-bar control
   └─ active meeting detected → follow Teams title bar
      ├─ default → 92 × 30 cells, 16 ms hover → 272 × 46 curved-root island
      ├─ --inline-titlebar → 92 × 29 → 272 × 29 disclosure below the frame outline
      ├─ narrow title bar → 46 × 30 camera-only caption cell
      └─ shared content detected → start Windows Graphics Capture
         ├─ authoritative UIA shared-content BoundingRectangle
         ├─ cropped-only reusable CPU buffer
         └─ one low-frequency latest-frame backup
            ├─ Windows clipboard
            ├─ optional Windows Screenshots PNG
            └─ click-through flash overlay
```
