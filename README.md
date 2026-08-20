# Snapbar

Microsoft Teamsで他の参加者が共有している画面の**共有コンテンツ部分だけ**を、フローティングバーから1クリックでWindowsクリップボードへコピーする軽量アプリです。

## 動作方針

- 必要なときだけ手動で起動し、閉じるとプロセスも終了します。
- 撮影結果はWindowsクリップボードだけへコピーします。自動保存、アップロード、テレメトリ、常駐処理はありません。
- 成功時にコピーするのは検出済みの共有コンテンツ領域だけです。Teamsウィンドウ全体へフォールバックしません。
- 共有領域を十分な確度で特定できない場合は、誤った範囲をコピーせずエラーにします。

## 共有コンテンツ領域の自動検出

固定ピクセルや固定割合では切り抜きません。撮影のたびに次の順で領域を判定します。

1. Windows UI Automationから、Teams内の「共有画面」「shared content」「presentation」などに対応するUI要素と画面座標を取得します。
2. UI Automationの画面座標を、その時点の実キャプチャ画像サイズへ整数比で変換します。DPI、拡大率、負座標を含むマルチモニター、ウィンドウサイズの違いを吸収します。
3. UI要素の意味、面積、中央寄りか、複数サンプル点から同じ領域が得られたかを評価し、チャット、参加者一覧、ツールバー、会議操作部を除外します。
4. 標準モードでは、UI Automationだけで決まらない場合に限り、画像上の大きな内側矩形を解像度非依存で検出します。
5. 共有映像の上下左右にTeams背景と同色のレターボックスがある場合は、その連続帯だけを追加で除去します。

クロップアイコンは、自動切り抜きの方式を切り替えます。

- **オフ（標準）**: UI Automationを優先し、確度の高い画像解析フォールバックも使用します。
- **オン（厳格）**: UI Automationで共有領域を確認できた場合だけ撮影します。

どちらのモードでも、ウィンドウ全体をコピーする動作はありません。

## 操作

1. Teamsで会議または共有画面を表示します。ポップアウト表示にも対応します。
2. `Snapbar.exe`を起動します。
3. 左端のウィンドウアイコンでTeams候補を再検出し、複数ある場合は対象を切り替えます。
4. 必要に応じてクロップアイコンで標準／厳格モードを切り替えます。
5. 右端の赤い撮影ボタンを押します。成功すると画像がクリップボードへ入り、チェック表示になります。
6. PowerPoint、OneNote、チャットなどへ貼り付けます。

ロックアイコンはバーの位置固定、`…`は対象情報・再検出・終了メニューです。

## ビルド

Windows 11とRust stableを使用します。

```powershell
git clone https://github.com/Ray-d3v/Snapbar.git
cd Snapbar
cargo build --release
.\target\release\snapbar.exe
```

GitHub Actionsの`CI`ワークフローでも、Windows向けrelease exeをartifactとして生成します。

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
GPUI floating bar
    └─ capture request
        └─ xcap / Windows.Graphics.Capture
            └─ UI Automation semantic detection
                └─ conservative visual fallback
                    └─ detected shared-content crop
                        └─ arboard / Windows clipboard
```
