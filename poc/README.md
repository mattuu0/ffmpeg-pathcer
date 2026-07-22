# poc/ — 検証・実験用コード

ここに置かれているクレートは、いずれも `proxy/`（本体のプロキシ DLL）の
実装ではありません。設計判断の根拠となった実験や、別アプローチとの比較
検証のために残してある、使い捨て前提の検証プログラムです。

- **dda-probe** — ffmpeg も本体の proxy DLL も使わない、最小の Desktop
  Duplication API 検証プログラム。UAC 遷移時に `AcquireNextFrame` が
  実際にどう失敗するか、どう復旧させれば効果があるか（何もしない /
  再アタッチのみ / 同じ device で再複製 / device から丸ごと再構築）を
  戦略ごとに比較するために作った。ここで得た知見（"古いインスタンスを
  drop してから再複製する"順序が必須、等）が `proxy/src/hooks/pump.rs` の
  実装に反映されている。

- **desktop-shot** — `win_desktop_duplication` クレートを使い、secure
  desktop（Winlogon / UAC 同意プロンプト）を実際にキャプチャできるかを
  PNG 保存で確認するための検証プログラム。SYSTEM 権限で実行する前提。

- **capture-helper** — SYSTEM 権限で起動し、DDA でキャプチャしたフレームを
  直接 ffmpeg の標準入力にパイプする、プロキシ DLL とは別方式の検証実装。
  ddagrab フィルタ自体を使わず、独自にキャプチャ〜エンコードを行う経路が
  UAC 遷移にどう振る舞うかを比較するために作った。

- **deployer** — BtbN FFmpeg-Builds のダウンロードと DLL 差し替えを
  自動化する、検証環境セットアップ用の補助ツール。

- **test-harness** — 実際に `ddagrab` フィルタ付き ffmpeg を起動し、UAC
  同意プロンプトを実際にトリガーして、本体プロキシ DLL のログ
  （`ddagrab_proxy.log`）を確認する統合テストツール。

- **dummy-tcp-server** — ffmpeg の `-f ... tcp://127.0.0.1:PORT` 出力を受け
  取って捨てるだけの受信専用サーバー。受信バイト数・区間スループットを
  CSV に記録し、`verify_matrix.py` から配信スループットの比較に使う。

いずれも本体の動作には必要なく、`output/` 配下の成果物にも含まれません。

## run_all.py / verify_matrix.py — プロキシ有無 x 実行権限のパフォーマンス比較

`ddagrab_proxy` の有無と実行権限（通常 / SYSTEM、
[PAExec](https://www.poweradmin.com/paexec/) で昇格）を組み合わせた4パターン
（`proxy_normal` / `proxy_system` / `noproxy_normal` / `noproxy_system`）で
ffmpeg を実行し、`dummy-tcp-server` への送信スループットと ffmpeg 自身の
dup/drop/speed ログを比較するための検証スクリプトです（UAC 同意プロンプトの
遷移自体は検証対象に含めず、単純なパフォーマンス比較のみを行います）。

Python 3（`python` コマンド、Windows なら `py` でも可）が必要です。バッチ
ファイル特有のコードページ/改行コード問題を避けるため、この検証一式は
すべて Python スクリプトとして実装しています。

### 実行方法（推奨）— run_all.py 1本で完結

管理者権限のコマンドプロンプト / PowerShell から、次を実行してください。

```
python poc\run_all.py
```

`dummy-tcp-server` のビルド（未ビルド時のみ）から4パターンの実行、結果
サマリーの書き出しまで、これ1本で完結します。事前に `cargo build` 等を
手動で実行しておく必要はありません。

`ffmpeg-master-latest-win64-lgpl-shared/bin/paexec.exe` は既に配置済みである
ことが前提です。

### verify_matrix.py を直接実行する場合

すでに `dummy-tcp-server` をビルド済みなら、管理者権限で次を直接実行しても
構いません（`run_all.py` はビルドチェックを挟んでこれを呼び出しているだけ
です）。

```
python poc\verify_matrix.py
```

実行前の `bin/avfilter-12*.dll` の配置は自動的に退避され、全パターン終了後に
元の状態へ復元されます。

結果は `poc\verify-matrix\results\<タイムスタンプ>\<パターン名>\` 以下に
`ffmpeg.log`（dup/drop/speed 等）、`dummy_tcp_server.csv`（受信スループット）、
（プロキシ利用時のみ）`ddagrab_proxy.log` として保存されます。

全パターン完了後、`poc\verify-matrix\summarize.py` が自動実行され、4パターン
分の dup/drop/speed・受信バイト数を1つにまとめた
`poc\verify-matrix\results\<タイムスタンプ>\summary.txt` を生成します。

## start_stream.py — 実際にAndroid端末等へRTSP配信する

`poc\start_stream.py` は、検証ではなく実際にデスクトップを RTSP で配信する
スクリプトです。

**別途 RTSP サーバー（[MediaMTX](https://github.com/bluenviron/mediamtx) 等）
を先に起動しておく必要があります。** ffmpeg の `rtsp` マクサー（出力側）
には listen/server モードが無く（`rtsp_flags listen` は入力側専用 —
`ffmpeg/libavformat/rtsp.c` でこのフラグ定数が `DEC` 専用として登録されて
おり、出力側の `rtspenc.c` では一切参照されていないことを確認済み）、この
スクリプトは ffmpeg から RTSP サーバーへ PUSH するだけです。

MediaMTX はデフォルト設定のまま `mediamtx.exe` を実行するだけで
`rtsp://0.0.0.0:8554` で立ち上がります。

起動時にプロキシ有無・実行権限を選べます。

```
python poc\start_stream.py --proxy --normal
python poc\start_stream.py --proxy --system
python poc\start_stream.py --no-proxy --normal
python poc\start_stream.py --no-proxy --system
```

RTSP サーバーが別マシン、または既定と異なるポートで動いている場合は
`--server-host` / `--server-port` で指定してください（既定値は
`127.0.0.1:8554`）。

管理者権限で実行してください。起動時に RTSP サーバーへの疎通を確認し、
繋がらなければ先に起動するよう案内して終了します。接続先の URL
（`rtsp://<このPCのIP>:8554/live`）が表示されるので、同じ LAN 内の Android
端末で VLC アプリを開き、ネットワークストリームとしてその URL を指定すれば
視聴できます。

フォアグラウンドで動き続けるので、配信を止めるときはこのスクリプトを実行
しているターミナルで Ctrl+C を押してください（`--system` 利用時も、PAExec
経由で起動した SYSTEM 権限の ffmpeg プロセスまで含めて後始末します）。
DLL の配置は `verify_matrix.py` と同様に自動退避・復元されます。
