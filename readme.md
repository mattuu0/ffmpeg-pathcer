# ddagrab_proxy

FFmpeg の `ddagrab`（Windows Desktop Duplication API）フィルタが、UAC の
同意プロンプト表示（secure desktop / Winlogon への遷移）をまたいでも
キャプチャを継続できるようにするプロキシ DLL です。

DLL の使い方は [`output/README.md`](output/README.md) を参照してください。

A proxy DLL that patches FFmpeg's `ddagrab` (Windows Desktop Duplication API)
filter so capture keeps running across a UAC consent prompt (a transition to
the secure desktop / Winlogon).

See [`output/README.md`](output/README.md) for usage instructions.

## ⚠️ 免責事項 / DISCLAIMER

**本ソフトウェアは無保証（AS IS）で提供されます。使用によって何が起きても、
作者・貢献者・配布者は一切の責任を負いません。** 利用するかどうか、利用に
よって生じた結果はすべて利用者自身の責任です。

**このソフトウェアに、認証回避・権限昇格・自己増殖・隠蔽機能などの危険な
コードは含まれていません。** 使っているのは公開の Windows API
（DXGI/Direct3D11）と、通常の DLL エクスポート転送だけです。

**悪意のある使用は厳禁です。** 相手の同意なく端末に無断でインス
トールすること、このソフトが何をするか偽って配布・使用させること、不正ア
クセスなど法令に触れる目的での使用はできません。

**本ソフトウェアは FFmpeg 本体を改変・同梱するものではなく、その配布物
（LGPL/GPL 版バイナリ）が読み込む DLL を差し替えるだけの補助ツールです。**
FFmpeg 本体のライセンス（LGPLv2.1+ もしくはビルド構成により GPLv2+/GPLv3+）
はそのまま適用されます。ご自身が使う FFmpeg 配布物のライセンス条件は各自
ご確認ください。

**本ソフトウェアをダウンロード・使用・組み込みした時点で、この免責事項に
同意したものとみなします。**

---

**This software is provided AS IS, with no warranty. The author(s),
contributor(s), and distributor(s) accept no liability whatsoever for
anything that happens from using it.** Whether to use it, and everything
that results from that use, is entirely the user's own responsibility.

**This software contains no dangerous code** — no credential bypass,
privilege escalation, self-propagation, or concealment mechanisms. It only
uses public Windows APIs (DXGI/Direct3D11) plus ordinary DLL export
forwarding.

**Malicious use is strictly forbidden.** This includes installing it on
someone else's device without their consent, distributing or deploying it
while misrepresenting what it does, or any use that constitutes unauthorized
access, voyeurism, stalking, or otherwise breaks the law. Building it into
remote-desktop or remote-support tools that record/monitor another person's
device with proper, informed consent is fine.

**This software does not modify or bundle FFmpeg itself** — it's a helper
tool that only replaces a DLL your own FFmpeg distribution (LGPL/GPL binary)
loads. FFmpeg's own license (LGPLv2.1+, or GPLv2+/GPLv3+ depending on build
configuration) still applies in full; check the license terms of whatever
FFmpeg distribution you use.

**By downloading, using, or embedding this software, you agree to this
disclaimer.**

## 構成 / Structure

- `proxy/` — 本体。`avfilter-12.dll` の全 export をフォワーディングしつつ、
  DXGI の Desktop Duplication 呼び出しだけをフックしてリカバリ処理を行う
  プロキシ DLL。
- `export-scan/` — DLL の named export 一覧を読み取るためのライブラリ
  （`proxy/build.rs` が .def ファイル生成に使用）。
- `xtask/` — `export-scan` の動作確認用 CLI（開発補助ツール）。
- `output/` — ビルド済み DLL と使い方ドキュメントの配置先。
- `poc/` — 検証・実験用のコード一式。本体の実装ではなく、設計判断の根拠と
  なった実験や、別方式との比較検証のために残してあります。詳細は
  [`poc/README.md`](poc/README.md) を参照してください。

- `proxy/` — The main crate: the proxy DLL that forwards every export of
  `avfilter-12.dll` unchanged while hooking only the DXGI Desktop
  Duplication calls to perform recovery.
- `export-scan/` — Library that reads a DLL's named exports (used by
  `proxy/build.rs` to generate its `.def` file).
- `xtask/` — CLI wrapper around `export-scan` for manual inspection (dev
  helper tool).
- `output/` — Where the built DLL and its usage documentation live.
- `poc/` — Standalone experiments and proof-of-concept code, not part of
  the shipped proxy. Kept around because it's what several design decisions
  are based on; see [`poc/README.md`](poc/README.md) for details.

## 動作原理の要点 / How it works

- ddagrab 自身が呼ぶ `DuplicateOutput`/`DuplicateOutput1` をフックし、
  本物の `IDXGIOutputDuplication` を裏側の専用スレッド（pump）に渡します。
- pump スレッドは本物のインスタンスを自分のペースでポーリングし続け、
  UAC 遷移などで `ACCESS_LOST` になった場合は「古いインスタンスを先に
  drop してから再生成する」順序を守って復旧します。
- ddagrab 自身が呼ぶ `AcquireNextFrame` は、pump が GPU 上にキャッシュした
  最新フレームを返すダミーの `IDXGIOutputDuplication` 実装（プロキシ）
  から返されるため、ddagrab 側からは常に「フレームがまだ来ていないだけ」
  にしか見えず、復旧処理そのものを意識しません。

- ddagrab's own calls to `DuplicateOutput`/`DuplicateOutput1` are hooked, and
  the real `IDXGIOutputDuplication` is handed off to a dedicated background
  thread ("pump").
- The pump thread polls the real instance at its own pace, and on
  `ACCESS_LOST` (e.g. during a UAC transition) recovers by dropping the dead
  instance FIRST, then re-duplicating -- that ordering turned out to be
  required for recovery to actually work.
- ddagrab's own `AcquireNextFrame` calls are served by a stub
  `IDXGIOutputDuplication` implementation that returns whatever frame the
  pump most recently cached on the GPU, so ddagrab itself only ever sees
  "no new frame yet" and never has to know recovery is happening at all.
