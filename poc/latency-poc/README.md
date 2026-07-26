# latency-poc — ストリームプロトコル/エンコード遅延計測 POC

Windows (キャプチャ + エンコード + 送信) と Android TV / Fire TV (受信 +
デコード + 表示) の2つで構成される、同一LAN内での低遅延デスクトップ
ストリーミングの検証用 POC です。ストリームプロトコル・エンコード・
デコードそれぞれの遅延要因を切り分けて観測することが目的です。

既存の `poc/start_stream.py`（RTSP + MediaMTX 経由）とは別に、**RTSP
サーバーなしで** Windows から Android へ配信する方式です。

## 構成

```
poc/latency-poc/
  windows-sender/     Windows側: ddagrab + h264_nvenc/hevc_nvenc + TCP送信
  tcp-ws-relay/        中継サーバー(Rust): TCP受信 → WebSocketクライアントとしてAndroidへ転送
  README.md           このファイル
poc/android-viewer/tv/  Android側: WebSocketサーバー(listen) + MediaCodec(H.264/HEVC自動判定)デコード + 表示
```

## 全体の流れ

```
Windows (ffmpeg)
  → TCP → tcp-ws-relay（自動起動、send_stream.py と同じPC上で動く）
    → WebSocket → Android（WebSocketServer が listen、mDNSで自分をアドバタイズ）
```

- ffmpeg は最初から最後まで **生の TCP** を喋るだけです。WebSocket化は
  `tcp-ws-relay` が担い、ffmpeg 自身は何も変わりません。
- `send_stream.py` は実行すると **`tcp-ws-relay` を自動的に子プロセスとして
  起動し**、ffmpeg をそのリレーの localhost ポートに接続させます。
  ユーザーがリレーを別途手動起動する必要はありません。
- **Android 側が WebSocket サーバー（listen 側）**です。元々の直接TCP方式と
  同じ役割分担（Android が listen して mDNS で自分をアドバタイズし、
  相手から接続しにきてもらう）を保ったまま、間に TCP↔WebSocket 変換の
  `tcp-ws-relay` を挟んだ形です。
- `tcp-ws-relay` が **WebSocket クライアント**です。起動すると mDNS で
  Android を自動的に探し、見つかったら WebSocket で接続しにいきます。

## プロトコル概要

- 映像コーデックは **H.264 または HEVC (H.265)**（Windows側 `--codec` で
  選択、既定は HEVC）、伝送は **生の Annex-B バイトストリームをそのまま
  流すだけ**です（RTP/RTSP のようなパケット化・セッションネゴシエー
  ションは一切なし）。
- 元々は RTP/UDP でしたが、UDPはパケットロスがあるとNALが歯抜けになり、
  MediaCodecがそれをエラーにせず「ブロックノイズ状の破損映像」や
  「フレームが全く出てこない」形で表面化していました。TCPに変更した
  ことで、再送・順序保証がプロトコル層で担保され、受信側が受け取る
  バイトストリームは常に完全かつ順序通りになります（トレードオフとして、
  パケットロスがあった場合はコマ落ちではなく若干の遅延増加という形で
  現れます）。
- SDP のようなセッション自体が無いため、`-bsf:v dump_extra=freq=keyframe`
  でエンコーダのパラメータセット（HEVCなら VPS/SPS/PPS、H.264なら
  SPS/PPS）を **毎キーフレームの直前にインラインで再挿入**しています。
  これにより Android 側は接続タイミングに関係なく、次のキーフレームが
  来た時点でデコーダを構成できます。
- **Android側はどちらのコーデックが流れてきたか自動判定します**
  （再ビルド・手動切り替え不要）。最初に受信したパラメータセットNAL
  （HEVCのVPS、またはH.264のSPS/PPS）で一度だけ判定し、その接続が続く
  間は固定されます。新しい接続が張られると、その時点で再度判定し直され
  ます。
- Android 側は受信したバイト列を Annex-B のスタートコード
  （`0x000001` / `0x00000001`）で NAL 単位に分割するだけの単純な実装です
  （外部ライブラリ不使用、`AnnexBNalSplitter`）。TCP受信・WebSocket受信の
  どちらでも同じ分割ロジックを共有しています。
- `tcp-ws-relay` は受け取ったTCPバイト列を**そのままバイナリの
  WebSocketフレームとして**Androidへ転送します（NAL境界に合わせた
  再チャンク化はしません — Android側のNAL分割ロジックはどのみちバイト列の
  連続ストリームとして扱うので、チャンクの切れ目とNAL境界が一致していなくても
  問題ありません）。
- Android側の`WebSocketServer`はRFC 6455のWebSocketハンドシェイク・
  フレーミングを外部ライブラリなしで自前実装しています（このPOCは
  RTPデパケタイザやAnnex-Bスプリッタも同様に自前実装している方針の
  延長です。バイナリフレームの受信のみをサポートし、フラグメント化
  されたメッセージには対応していません — `tcp-ws-relay`もフラグメント化
  しないため実運用上問題ありません）。
- ddagrabのキャプチャはフルレンジBT.709ですが、NVENCは色域/レンジの
  出力オプション（`-color_range`等）を無視するため、`h264_metadata`/
  `hevc_metadata` bitstream filterでビットストリームのVUIを直接
  `video_full_range_flag=1`・BT.709に書き換えています。これを怠ると
  Android側がリミテッドレンジ前提でデコードし、黒が浮き白が沈む
  「色褪せ」た映像になります（実機で確認済みの不具合。ただし現状は
  これでも完全には解消していません — Fire TV機種のデコーダがVUIや
  MediaFormatの色ヒントを尊重しない既知の問題の可能性があります）。

## 使い方

### 1. tcp-ws-relay をビルド（初回のみ）

```
cargo build --release -p tcp-ws-relay
```

`send_stream.py` がこのバイナリ（`target\release\tcp-ws-relay.exe`）を
自動的に起動するので、以降ユーザーが手動で実行する必要はありません。

### 2. Android 側（先に起動しておく）

`poc/android-viewer` を Android Studio で開き、`tv` モジュールを Fire TV
（またはAndroid TVエミュレータ、実機）にインストールして起動してください。

```
cd poc\android-viewer
.\gradlew.bat :tv:installDebug
```

起動すると "○ WAITING FOR STREAM (WEBSOCKET 5001)" と表示され、WebSocket
ポート5001番で接続待ち受けを開始すると同時に、mDNS/NSD（サービスタイプ
`_latencypoc._udp.`、インスタンス名 `latencypoc-viewer`）で自分自身を
アドバタイズします。IPアドレスを手動で調べる必要はありません。

このアプリはリスナーとして起動しっぱなしにできます。`tcp-ws-relay`（や
Windows側の送信）を何度再起動しても、その都度新しい接続を受け付けます。

### 3. Windows 側

初回のみ、`zeroconf` パッケージ（`--no-relay`モードでのみ使用）を
インストールしてください。

```
pip install -r poc\latency-poc\windows-sender\requirements.txt
```

管理者権限の PowerShell / コマンドプロンプトから:

```
python poc\latency-poc\windows-sender\send_stream.py --output-idx 1 --fps 60 --bitrate 15M --codec hevc
```

これだけで、`tcp-ws-relay` の起動（AndroidをmDNSで自動探索してWebSocket
接続） → ffmpeg のキャプチャ/エンコード開始、まで一通り行われます。

主なオプション:

- `--no-proxy` — `ddagrab_proxy.dll`ではなく素の`avfilter-12.dll`を使う
  （既定はプロキシ有効。下記「DDA復旧ロジック（ddagrab_proxy.dll）」参照）
- `--relay-tcp-port N` — リレーがffmpegの接続を待ち受けるTCPポート（既定: 5000）
- `--android-ws-url ws://<ip>:<port>` — Androidの固定WebSocketアドレスを
  明示指定（省略時はリレーがmDNSで自動探索）
- `--relay-discovery-timeout N` — リレーがAndroidをmDNSで探すタイムアウト秒数（既定: 10秒）
- `--codec hevc|h264` — 映像コーデック（既定: hevc）。Android側は再起動不要で自動判定します
- `--output-idx N` — キャプチャする配信モニターを指定（既定: 0 = プライマリ）
- `--fps N` — キャプチャ/エンコードのフレームレート（既定: 60）
- `--bitrate 8M` — CBR ターゲットビットレート（既定: 8M）
- `--width` / `--height` — キャプチャ解像度（既定: 1920x1080）
- `--no-relay` — リレーを使わず、Android自身のTCPリスナーに直接接続する
  従来モード（下記「--no-relay: 直接TCPモード」参照）

実行中は ffmpeg 自身の `-stats` 出力をパースし、以下を1秒間隔程度で
表示します:

```
[STATS] t=   12.3s frame=   370 fps=  60 bitrate=   7823.1kbits/s drop=   0 dup=   0 speed=1.00x (speed<1x means encoder is behind realtime)
```

`speed` が 1.00x を切っている場合はエンコーダがリアルタイムに追いつけて
いない（＝エンコード遅延がここで発生している）ことを意味します。リレー
自身の `[INFO]`/`[WARN]` ログは `[relay]` プレフィックス付きで同じ
コンソールに表示されます。

Ctrl+C で送信を停止すると、ffmpeg と `tcp-ws-relay` の両方が終了します。

### 4. Android 側の表示

映像が表示されると同時に、左上に以下の統計オーバーレイが出ます:

- **FPS** — 実際にデコード〜レンダリングされたフレームレート
- **Bitrate** — 直近の受信ビットレート
- **NAL interval** — 連続する NAL の到着間隔（送信側時刻の埋め込みが
  無いため、単純な受信側の到着間隔です。ネットワークが詰まっている/
  バーストしているかの目安であり、絶対的な片道遅延ではありません）。
- **Decode latency** — `MediaCodec` にアクセスユニットを投入してから
  `onOutputBufferAvailable` コールバックが実際に発火するまでの時間
  （ハードウェアデコーダのみの遅延。Surfaceへの表示自体は
  `releaseOutputBuffer(..., render=true)` で即座にスケジュールされます）

## DDA復旧ロジック（ddagrab_proxy.dll）

`send_stream.py`は既定で、素の`avfilter-12.dll`ではなく本体の
`ddagrab_proxy.dll`（`poc/start_stream.py`・`poc/verify_matrix.py`が
使っているのと同じDLL、`poc/dll_layout.py`のスタッシュ/適用/復元ロジック
経由）を使うようにしています。

これは実際に長時間（約115秒、6942フレーム）配信を続けたところ、以下の
エラーでffmpegが正常終了（exit code 0）してしまう不具合を確認したための
対応です:

```
[Parsed_ddagrab_0 @ ...] DDA ReleaseFrame failed!
[Parsed_ddagrab_0 @ ...] EOF timestamp not reliable
[in#0/lavfi @ ...] Error during demuxing: Generic error in an external library
```

`DDA ReleaseFrame failed!`はDesktop Duplication API
（`IDXGIOutputDuplication::ReleaseFrame()`）のセッションが何らかの理由
（UAC遷移・ロック画面・リモートデスクトップのセッション変化など）で
無効化されたときに出るエラーです。素の`avfilter-12.dll`はこれを単に
入力終了として扱い、ffmpegはそのまま（クラッシュではなく）正常終了して
しまいます。

`ddagrab_proxy.dll`はこの手のDDAセッション遷移から復旧するために本体
（`dda-hook-core/`）が実装しているロジックで、`poc/dda-probe`での検証を
経て作られたものです。`send_stream.py`でもこれを有効にすることで、
長時間配信中にセッション遷移が起きても復旧を試みられるようにしています。
`--no-proxy`で従来通り素の`avfilter-12.dll`（復旧ロジックなし）に戻せます。

DLLの入れ替え自体は`poc/dll_layout.py`の実績あるスタッシュ/復元ロジックを
再利用しており、`send_stream.py`の実行前後で`bin/avfilter-12.dll`の状態が
必ず元通りに復元されます（Ctrl+C・エラー・mDNS探索失敗など、どの終了経路
でも同様です）。

### `ddagrab_proxy.dll`は薄いシムに過ぎない — `dda_hook_core.dll`が別途必要

**プロキシを有効にしても復旧しなかった（`DDA ReleaseFrame failed!`から
そのままEOF終了する）不具合が実際に発生**しました。原因は、
`ddagrab_proxy.dll`自体は`proxy/src/lib.rs`にある通り**薄いシム**であり、
実際のDDAフック・復旧ロジックを持つ`dda_hook_core.dll`を自分と同じ
ディレクトリから`LoadLibrary`で動的に読み込む設計だったためです。
`ffmpeg-master-latest-win64-lgpl-shared/bin/`に`dda_hook_core.dll`が
一切配置されていなかったため、プロキシは「有効」になっていても
`LoadLibrary`が実質何もフックできず、DDAセッション遷移からの復旧が
一度も行われていませんでした。

これを踏まえ、`paths.py`の`check_proxy_dll()`が`dda_hook_core.dll`の
存在も確認し、`output/dda_hook_core.dll`から`bin/`へ**自動的にコピー**
するようにしています（ビルド自体は自動化していません — 下記コマンドで
手動ビルドが必要です）。

`ddagrab_proxy.dll`/`dda_hook_core.dll`をビルドし直す場合:

```
$env:DDAGRAB_REAL_AVFILTER_DLL = "<正規のavfilter-12.dllのパス>"
$env:DDAGRAB_LIB_EXE = "<MSVC Build Toolsのlib.exeのフルパス>"
cargo build --release -p ddagrab_proxy -p dda_hook_core
```

**注意**: `DDAGRAB_REAL_AVFILTER_DLL`は「フォワード先のファイル名」を
このパスの**ファイル名から**導出します（例: `avfilter-12.dll`を渡すと
プロキシは`avfilter-12_orig.dll`へフォワードするようビルドされる）。
正規版のDLLが既に`avfilter-12_orig.dll`のような別名になっている場合、
一旦`avfilter-12.dll`という名前でコピーしたものを指定してください
（さもないと `avfilter-12_orig_orig.dll` のような二重サフィックスの
名前を期待するビルドになり、`dll_layout.py`の想定と食い違います）。

ビルド後は次の場所へ配置してください（`send_stream.py`は`output/`から
`bin/`への`dda_hook_core.dll`コピーだけ自動化しています。
`output/ddagrab_proxy.dll`自体の更新は手動です）:

```
Copy-Item target\release\ddagrab_proxy.dll output\ddagrab_proxy.dll
Copy-Item target\release\dda_hook_core.dll output\dda_hook_core.dll
```

## --no-relay: 直接TCPモード（比較用）

`tcp-ws-relay` を経由しない、Android自身がTCPを直接listenする元々の
方式も比較用に残しています（`TcpReceiver.kt`はそのまま残しており、
`MainActivity.kt`の配線を`WebSocketServer`から`TcpReceiver`+
`NsdAdvertiser`に入れ替えてビルドし直せば使えます）。Windows側は:

```
python poc\latency-poc\windows-sender\send_stream.py --no-relay
python poc\latency-poc\windows-sender\send_stream.py --no-relay --dest 192.168.1.50 --port 5000
```

`--no-relay` はmDNSで直接Androidアプリを探す元の動作（`--dest`省略時）、
または`--dest`/`--port`を明示して固定アドレスへ接続する動作のどちらも
サポートしています。

## 低遅延化のための追加チューニング

安定して動くようになった後、さらに遅延を削るために以下を行っています:

- **キーフレーム間隔を1秒→4秒に延長**。キーフレームはCBRの下では他の
  フレームよりずっとサイズが大きく、送出のたびに瞬間的な遅延スパイクに
  なっていたため、頻度を下げてスパイクの回数自体を減らしています。
  当初は`-intra-refresh`（UDPのパケットロス耐性のための仕組み。1スライス
  分の映像だけを失う代わりに常時ビットレートの一部を消費し続ける）も
  TCP化により不要と判断し無効化しましたが、4秒のGOPだとPフレームだけで
  維持する期間が長くなり、CBRのビット上限下で画質のにじみ・ブロック
  ノイズがじわじわ蓄積してから次のキーフレームで一気に解消される形に
  なり、「滑らかさが落ちた」体感につながりました。intra-refreshを
  再度有効化し、GOP全体を通して継続的に画質を補正する方が実際の見え方は
  良かった（ユーザー確認済み）ため、キーフレーム間隔は4秒のまま
  intra-refreshは有効に戻しています。
- **TCP送信ソケットの送信バッファを64KiBに制限**（`send_buffer_size`）。
  既定のままだと、OS側が数フレーム分を黙ってバッファしてから送出できて
  しまい、エンコーダとワイヤーの間に見えない遅延（キューイング）が
  発生します。LAN内の高速なリンクではこのバッファは throughput 平滑化の
  役に立たないため、小さく制限しています。
- **Android側のMediaCodecを同期ポーリングから非同期コールバック
  （`MediaCodec.Callback` / `setCallback`）に変更**。以前は入力バッファの
  空き待ちに最大10msブロックし、出力の確認も投入直後の1回きりだった
  ため、実際にフレームが出力される瞬間より前にしか計測できず、また
  フィーダースレッドが不要に待たされることがありました。コールバック
  方式では、入力バッファが空いた瞬間・出力が用意できた瞬間にそれぞれ
  即座に反応するため、フィーダースレッドの無駄待ちが無くなり、
  `Decode latency` の計測精度も上がっています。

## mDNS/NSDの仕組み（tcp-ws-relay版）

Android側の`NsdAdvertiser`は、元々の直接TCP方式のときと**全く同じ**
mDNS/NSD サービスタイプ・インスタンス名（`_latencypoc._udp.`、
`latencypoc-viewer`）で自分自身（WebSocketのリスニングポート）を
アドバタイズします。

`tcp-ws-relay`はこのサービスをmDNSで発見し（`poc/latency-poc/tcp-ws-relay/src/main.rs`
の`discover_android()`）、見つかったアドレス・ポートに対して
`ws://<host>:<port>`でWebSocketクライアントとして接続します。

- Windows側の`send_stream.py`は変更なしで動作（自分で起動したリレーの
  localhostアドレスを直接知っているので、そもそもmDNS探索は使いません
  — mDNS探索を行うのはリレー自身です）
- `--android-ws-url`でmDNS探索をスキップし、固定アドレスへ接続することも
  できます

以前の設計（`tcp-ws-relay`が自分自身をアドバタイズし、Android側がそれを
探しにいく方式）では、リレーが自分のLAN上のIPアドレスを正しく検出できず
（VPN/トンネルインターフェースのアドレスを拾ってしまう等）、Android側が
発見できない問題がありました。Android自身が（元々そうだったように）
自分のアドレスを一番正確に知っているため、アドバタイズの主体をAndroid側に
戻すことでこの問題を回避しています。

## Fire TV (Android 9 / API 28) について

`tv` モジュールは `minSdk = 28` / `targetSdk = 28` に設定しており、
Fire TV Stick (第1/第2世代) や Fire TV Cube (第1世代) など Android 9 が
OS上限の機種でも動作します。`compileSdk` はビルドツール要件のため 37 を
使用していますが、これはコンパイル時のAPI参照可否のみに影響し、実機の
挙動には影響しません。

デコードは `MediaCodec` のハードウェアデコーダ（`MediaCodec.createDecoderByType`
が選ぶプラットフォームデフォルト。`VideoDecoder`が自動判定したコーデックに
応じて`"video/avc"`または`"video/hevc"`を渡します）を使用し、出力は直接
`Surface` に描画します（`releaseOutputBuffer(index, true)`）。ソフトウェア
デコードへのフォールバックは実装していません。**Fire TV機種のHEVC
ハードウェアデコード対応状況は世代によって差がある**ため、HEVCで
`MediaCodec.createDecoderByType`が失敗する（対応していない）機種では、
Windows側を `python send_stream.py --codec h264` で起動し直してください
（Androidアプリの再ビルド・再インストールは不要 — 新しい接続の最初の
パラメータセットNALでH.264と判定され、以後そちらでデコードします）。

HEVCのSPSは幅・高さの解析にH.264よりずっと複雑なビットストリーム構造
（参照ピクチャセットなど）を要するため、このPOCでは（H.264側も含め）
SPSから解像度をパースせず、`MediaFormat`初期化時は固定デフォルト
（1920x1080）を渡しています。`MediaCodec`自体はcsd-0/1から実際の解像度を
認識して描画するため、実際の表示解像度には影響しません。

## 既知の制約（POCゆえの割り切り）

- TCPのため、パケットロスや輻輳が起きても映像が壊れることはありません
  が、再送待ちの分だけ遅延が増える形で影響が出ます（UDPと違い、遅延と
  安定性のトレードオフの向きが変わっただけで、ネットワーク自体の問題を
  解消するわけではありません）。
- Android・Windows間の時刻同期を行っていないため、真の片道ネットワーク
  遅延（glass-to-glassの一部）は測定できません。
- 複数クライアント同時視聴は非対応です（Android側は1つのWebSocket接続
  のみを処理する設計で、`tcp-ws-relay`もAndroidへの接続は1本のみです）。
- 接続が切れた場合、Android側の`WebSocketServer`は次の接続を自動的に
  待ち受けます（`accept()`をループしているため）が、`tcp-ws-relay`は
  WebSocket切断時の自動再接続を実装していません（切断時は
  `send_stream.py`ごと再実行してください）。
- `tcp-ws-relay`経由の分、中継プロセスがもう1段挟まる分だけ、原理的には
  遅延がわずかに増えます（TCP→アプリ内バッファ→WebSocketフレーム化→
  Android側のWebSocketデコードという経路が追加されるため）。
