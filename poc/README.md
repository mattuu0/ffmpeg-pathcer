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

いずれも本体の動作には必要なく、`output/` 配下の成果物にも含まれません。
