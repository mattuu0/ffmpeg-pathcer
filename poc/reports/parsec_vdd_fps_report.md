# Parsec VDD（仮想ディスプレイ）でのフレームレート低下 調査レポート

## 背景・目的

ユーザーから「Parsec VDD（Parsec Virtual Display Driver）で追加した仮想ディスプレイを
録画すると、フレームレートが大きく下がる」という報告があった。本レポートはこの現象の
再現検証を行った結果をまとめたもの。

このプロジェクトは `ddagrab`（ffmpegの `libavfilter/vsrc_ddagrab.c`、DXGI Desktop
Duplication API経由のデスクトップキャプチャフィルタ）に対して、`ddagrab_proxy.dll`
という独自のCOM vtableフックDLLをかぶせている（`avfilter-12.dll` を差し替え、実体は
`avfilter-12_orig.dll` にリネームしてexportをフォワードする方式）。目的はUAC secure
desktop遷移時に `IDXGIOutputDuplication::AcquireNextFrame` が
`DXGI_ERROR_ACCESS_LOST`/`ACCESS_DENIED` を返して落ちるのを検知し、同一デバイス上で
`DuplicateOutput1` を呼び直して裏側でこっそり再接続する（ddagrab本体には
`WAIT_TIMEOUT` としてしか見せない）こと。直近のコミット
(`3014b21 pumpスレッドを廃止しdduplicationを素通し方式に変更、UAC回復はデバイス再利用のみに統一`)
で、専用pumpスレッドを廃止し素通し方式に変更、UAC回復のフォールバック
(`recreate_from_scratch` = デバイスごと作り直す方式)を削除して、同一デバイス再複製の
みに一本化する変更を行い、実機のUAC遷移テストで正常動作を確認済み。

今回のPersec VDD低下報告はこの一連の修正とは別軸の、新規の疑義。

## 環境

- OS: Windows 11 Home 10.0.26200
- GPU: NVIDIA GeForce RTX 3060
- ディスプレイ構成（`Get-PnpDevice -Class Display` / WMI `WmiMonitorID` で確認）:
  - `\\.\DISPLAY1`: 物理モニタ（ASUS VG279、PNPDeviceID `DISPLAY\AUS2782\...`）、144Hz設定、1920x1080
  - `\\.\DISPLAY34`: **Parsec Virtual Display Adapter**（PNPDeviceID `DISPLAY\PSCCDD0\...`、
    WMI UserFriendlyName `ParsecVDA`）、144Hz設定、1920x1080
- `ddagrab` の `output_idx` との対応: 実際にそれぞれの `output_idx` で1フレームを
  `hwdownload,format=bgra` 付きでPNG保存して視認して確認した。
  - `output_idx=0` → 物理モニタ（VS Codeのウィンドウが写る）
  - `output_idx=1` → Parsec VDD（当初はデスクトップ壁紙のみ、後にYouTube再生画面）
- 現在デプロイされている `ddagrab_proxy.dll`: 225,792 bytes（上記コミット
  `3014b21` の内容でビルド済み、`recreate_from_scratch` 削除・pumpスレッドなし版）。
  本物の `avfilter-12_orig.dll` は 29,841,408 bytes。

## 検証方法

すべて同一マシン・同一プロキシDLLで、`output_idx` だけを切り替えて実施。
`ddagrab_proxy.log` を各テスト前に削除してから実行し、
`[DuplicationProxy::AcquireNextFrame] [stats/1s]` の統計ログ
（`calls`=1秒間の呼び出し回数、`hits`=実際にフレームを取得できた回数、
`timeouts`=`WAIT_TIMEOUT`だった回数、`recoveries`/`recovery_failures`=UAC相当の
回復試行回数）を突き合わせて確認した。

### テスト1: 短時間・無エンコードでの output_idx比較

```
ffmpeg -hide_banner -f lavfi -i "ddagrab=output_idx=<N>:framerate=60" -t 5 -f null -
```

- `output_idx=0`（物理モニタ）: `fps=60`（安定）
- `output_idx=1`（Parsec VDD、静止画=デスクトップ壁紙のみ）: `fps=58`

### テスト2: NVENCエンコード込み30秒、Parsec VDD・静止画

```
ffmpeg -hide_banner -f lavfi -i "ddagrab=output_idx=1:framerate=60:video_size=1920x1080" \
  -c:v hevc_nvenc -b:v 8000k -maxrate 35000k -bufsize 35000k -t 30 -f null -
```

結果: `frame=1738, fps=58, speed=0.997x`（30秒間安定）

プロキシログの内訳（抜粋、1秒ごと）:
```
calls=55 hits=27 timeouts=28 recoveries=0 recovery_failures=0
calls=58 hits=28 timeouts=30 recoveries=0 recovery_failures=0
calls=57 hits=29 timeouts=28 recoveries=0 recovery_failures=0
```

`hits` が `calls` の約半分しかない。しかし `recoveries=0`／`recovery_failures=0` なので
ACCESS_LOST/UAC相当のエラーは一切発生していない。単純に「新しい提示（Present）が
なかったので `WAIT_TIMEOUT` だった」というケースが約半分を占めていた。

### テスト3: NVENCエンコード込み30秒、Parsec VDD・YouTube再生中（動きあり）

ユーザーがVDD側のデスクトップでYouTube動画を実際に再生している状態で、テスト2と同じ
コマンドを実行。

結果: `frame=1754, fps=58, speed=0.997x`（30秒間安定、テスト2とほぼ同じ）

プロキシログの内訳（抜粋、1秒ごと）:
```
calls=60 hits=57 timeouts=3 recoveries=0 recovery_failures=0
calls=60 hits=58 timeouts=2 recoveries=0 recovery_failures=0
calls=56 hits=56 timeouts=0 recoveries=0 recovery_failures=0
calls=61 hits=60 timeouts=1 recoveries=0 recovery_failures=0
```

`hits` が `calls` のほぼ全数（56〜60/60）に回復している。つまり画面に実際の動き
（動画再生）があるときは、Desktop Duplication APIから正常に新規フレーム通知が
来ており、テスト2で `hits` が半分だったのは「本当に画面が静止していたから」という
仕様通りの挙動だったことが分かる。

## 結論

**今回の検証範囲では、「Parsec VDDで録画するとフレームレートが大きく低下する」という
現象を再現できなかった。** 動きのあるコンテンツ（YouTube再生）を表示した状態では、
Parsec VDD (`output_idx=1`) でも物理モニタとほぼ同等の `fps≈58, speed≈0.997x` を
安定して維持しており、UAC回復ロジック（`recoveries`/`recovery_failures`）も一切
トリガーされていない。

静止画状態での `hits≈半分` という観測は、DXGI Desktop Duplication APIの一般的な
仕様（画面に変化がなければ新規フレームは供給されない）通りの挙動であり、Parsec VDD
固有の異常ではないと考えられる。

## 未検証・残された可能性

ユーザー報告の「大きく下がる」現象は、以下のいずれかの条件下でのみ再現する可能性が
あり、今回はいずれも検証できていない：

1. **Parsecの実際のストリーミングセッションが張られている状態**（別デバイス/別PCから
   実際にParsecクライアントで接続し、VDD経由で映像を受信している最中）。今回の検証は
   VDDが存在するだけで、実際のParsec配信セッションは張っていない。
2. **より長時間（数分〜)実行した場合の緩やかな劣化**。今回は最大30秒のみ確認。
   このプロジェクトでは過去に「pumpスレッドが常駐しNVENCと同じGPUデバイスコンテキスト
   に継続アクセスすることで、ddagrab本体のフレーム要求頻度が時間とともに低下する」
   という別の現象が確認されており（pumpスレッド廃止で解消済み、コミット `3014b21`）、
   同種の「時間経過で劣化する」現象がVDD特有の別要因で再発している可能性はゼロでは
   ない。
3. **異なる解像度・リフレッシュレート設定**、あるいは**複数ウィンドウ/複雑な描画内容**
   での再現。
4. **プロキシなし（本物の `avfilter-12_orig.dll` を `avfilter-12.dll` として直接使う）
   でも同じ低下が起きるかどうか**の比較。今回はプロキシ有効時のみで検証しており、
   プロキシなしでの比較は未実施（ユーザーからは「プロキシ有効時のみで発生」という
   申告があったため、プロキシに絞って検証したが、決定的な低下は再現しなかった）。

## 次に試すべきこと（申し送り）

1. 実際にParsecで別デバイスから接続し、ストリーミング中の状態で同じ
   `[stats/1s]` ログ採取を行う。
2. 数分〜10分程度の長時間キャプチャで `hits`/`timeouts` の推移を見て、時間経過による
   緩やかな劣化がないか確認する（`poc/loop_normal_verify.py` や
   `poc/verify_matrix.py` のパターンを `output_idx=1` 向けに応用できる）。
3. `--no-proxy`（`poc/dll_layout.py` の `apply_dll_mode` で本物avfilterに戻せる）
   でも同条件を試し、プロキシの有無で差が出るか確認する。
4. 再現した場合、`ddagrab_proxy.log` の `[stats/1s]` に加えて
   `DXGI_OUTDUPL_FRAME_INFO.AccumulatedFrames`（何フレーム分たまっていたか）や
   `LastPresentTime` も出力するようログを拡張すると、Parsec VDD側のPresent通知
   自体が間引かれているのか、DDA層で失われているのかを切り分けられる。
