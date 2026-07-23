# latency-poc — ストリームプロトコル/エンコード遅延計測 POC

Windows (キャプチャ + エンコード + 送信) と Android TV / Fire TV (受信 +
デコード + 表示) の2つで構成される、同一LAN内での低遅延デスクトップ
ストリーミングの検証用 POC です。ストリームプロトコル・エンコード・
デコードそれぞれの遅延要因を切り分けて観測することが目的です。

既存の `poc/start_stream.py`（RTSP + MediaMTX 経由）とは別に、**RTSP
サーバーなしで** Windows から Android へ直接 TCP でプッシュする方式です。

## 構成

```
poc/latency-poc/
  windows-sender/     Windows側: ddagrab + h264_nvenc/hevc_nvenc + TCP送信
  README.md           このファイル
poc/android-viewer/tv/  Android側: TCP受信 + MediaCodec(H.264/HEVC自動判定)デコード + 表示
```

## プロトコル概要

- 映像コーデックは **H.264 または HEVC (H.265)**（Windows側 `--codec` で
  選択、既定は HEVC）、伝送は **生の Annex-B バイトストリームを TCP で
  そのまま流すだけ**です（RTP/RTSP のようなパケット化・セッション
  ネゴシエーションは一切なし）。Android 側がリスナー（サーバー役）、
  Windows 側がそこへ接続しにいくクライアント役です。
- 元々は RTP/UDP でしたが、UDPはパケットロスがあるとNALが歯抜けになり、
  MediaCodecがそれをエラーにせず「ブロックノイズ状の破損映像」や
  「フレームが全く出てこない」形で表面化していました。TCPに変更した
  ことで、再送・順序保証がプロトコル層で担保され、Android側が受け取る
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
  間は固定されます。Windows側を別のコーデックで再起動して新しい接続を
  張り直すと、その時点で再度判定し直されます。
- Android 側は TCP ストリームを読みながら Annex-B のスタートコード
  （`0x000001` / `0x00000001`）で NAL 単位に分割するだけの単純な実装です
  （外部ライブラリ不使用）。RTP のようなパケット化を意識する必要が
  無くなった分、以前の RTP デパケタイザより大幅にシンプルになっています。
- ddagrabのキャプチャはフルレンジBT.709ですが、NVENCは色域/レンジの
  出力オプション（`-color_range`等）を無視するため、`h264_metadata`/
  `hevc_metadata` bitstream filterでビットストリームのVUIを直接
  `video_full_range_flag=1`・BT.709に書き換えています。これを怠ると
  Android側がリミテッドレンジ前提でデコードし、黒が浮き白が沈む
  「色褪せ」た映像になります（実機で確認済みの不具合）。

## 使い方

### 1. Android 側（先に起動しておく）

`poc/android-viewer` を Android Studio で開き、`tv` モジュールを Fire TV
（またはAndroid TVエミュレータ、実機）にインストールして起動してください。

```
cd poc\android-viewer
.\gradlew.bat :tv:installDebug
```

起動すると "○ WAITING FOR STREAM (TCP 5000)" と表示され、TCP 5000番
ポートで接続待ち受けを開始すると同時に、mDNS/NSD（サービスタイプ
`_latencypoc._udp.`、インスタンス名 `latencypoc-viewer`）で自分自身を
アドバタイズします。IPアドレスを手動で調べる必要はありません。

このアプリはリスナーとして起動しっぱなしにできます。Windows側の送信を
何度再起動しても、その都度新しい接続を受け付けます（Androidアプリ自体を
再起動する必要はありません）。起動中は画面が自動消灯しないよう
`FLAG_KEEP_SCREEN_ON` を設定しています。

### 2. Windows 側

初回のみ、`zeroconf` パッケージ（mDNS探索用）をインストールしてください。

```
pip install -r poc\latency-poc\windows-sender\requirements.txt
```

管理者権限の PowerShell / コマンドプロンプトから、`--dest` を省略して
実行すると、mDNSで Android 受信端末を自動検出してそこに接続します
（IPv4のみ対応。IPv6アドレスは無視します）。

```
python poc\latency-poc\windows-sender\send_stream.py
```

同一LAN上に Fire TV/Android TV が複数台あっても、この POC の受信アプリを
起動しているのが1台だけなら問題なく見つかります。mDNS が使えない
（ブロックされているなど）環境や、複数台が同時に起動していて対象を
固定したい場合は、`--dest`/`--port` を明示して自動検出をスキップできます:

```
python poc\latency-poc\windows-sender\send_stream.py --dest 192.168.1.50 --port 5000
```

主なオプション:

- `--dest IP` — 接続先IPv4アドレスを固定指定（省略時はmDNSで自動検出）
- `--port N` — 接続先TCPポートを固定指定（`--dest`省略時はmDNSで見つかったポートを使用、`--dest`指定時の既定値は5000）
- `--codec hevc|h264` — 映像コーデック（既定: hevc）。Android側は再起動不要で自動判定します
- `--discovery-timeout N` — mDNS探索のタイムアウト秒数（既定: 10秒）
- `--output-idx N` — キャプチャする配信モニターを指定（既定: 0 = プライマリ）
- `--fps N` — キャプチャ/エンコードのフレームレート（既定: 60）
- `--bitrate 8M` — CBR ターゲットビットレート（既定: 8M）
- `--width` / `--height` — キャプチャ解像度（既定: 1920x1080）

実行中は ffmpeg 自身の `-stats` 出力をパースし、以下を1秒間隔程度で
表示します:

```
[STATS] t=   12.3s frame=   370 fps=  60 bitrate=   7823.1kbits/s drop=   0 dup=   0 speed=1.00x (speed<1x means encoder is behind realtime)
```

`speed` が 1.00x を切っている場合はエンコーダがリアルタイムに追いつけて
いない（＝エンコード遅延がここで発生している）ことを意味します。

Ctrl+C で送信を停止できます（Android側は待ち受け状態に戻り、再度
send_stream.py を起動すればそのまま再接続できます）。

### 3. Android 側の表示

映像が表示されると同時に、左上に以下の統計オーバーレイが出ます:

- **FPS** — 実際にデコード〜レンダリングされたフレームレート
- **Bitrate** — 直近の受信ビットレート
- **NAL interval** — 連続する NAL の到着間隔（TCPにはRTPタイムスタンプの
  ような送信側時刻の埋め込みが無いため、単純な受信側の到着間隔です。
  ネットワークが詰まっている/バーストしているかの目安であり、絶対的な
  片道遅延ではありません）。
- **Decode latency** — `MediaCodec` にアクセスユニットを投入してから
  `dequeueOutputBuffer` が返るまでの時間（ハードウェアデコーダのみの
  遅延。Surfaceへの表示自体は `releaseOutputBuffer(..., render=true)` で
  即座にスケジュールされます）

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
- 複数クライアント同時視聴は非対応（1対1のTCP接続のみ）。
- 接続が切れた場合、Android側は次の接続を自動的に待ち受けます
  （`TcpReceiver`が`accept()`をループしているため）が、Windows側の
  自動再接続・再送ロジックは実装していません（切断時は`send_stream.py`
  を再実行してください）。
