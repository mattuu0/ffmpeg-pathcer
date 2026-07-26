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

- `dda-hook-core/` — フック本体。DXGI/D3D11 の Desktop Duplication 呼び出し
  をフックしてリカバリ処理を行う、FFmpeg/ddagrab に依存しない汎用 DLL。この
  DLL がプロセスにロードされるだけ（`DllMain` の `DLL_PROCESS_ATTACH`）で
  パッチが完了する。
- `proxy/` — `avfilter-12.dll` の全 export をフォワーディングしつつ、ロード
  時に `dda-hook-core` を `LoadLibrary` するだけの薄い「なりすまし」DLL。
  フック実装は一切持たない。他のソフトウェアに応用する場合は、この crate と
  同じパターン（対象アプリが読み込む DLL 名を模倣する export forwarding
  shim）を作れば `dda-hook-core` をそのまま再利用できる。
- `export-scan/` — DLL の named export 一覧を読み取るためのライブラリ
  （`proxy/build.rs` が .def ファイル生成に使用）。
- `xtask/` — `export-scan` の動作確認用 CLI（開発補助ツール）。
- `output/` — ビルド済み DLL と使い方ドキュメントの配置先。
- `poc/` — 検証・実験用のコード一式。本体の実装ではなく、設計判断の根拠と
  なった実験や、別方式との比較検証のために残してあります。詳細は
  [`poc/README.md`](poc/README.md) を参照してください。

- `dda-hook-core/` — The hook implementation itself: a generic DLL (no
  FFmpeg/ddagrab dependency) that hooks DXGI/D3D11 Desktop Duplication calls
  and performs recovery. Simply loading this DLL into a process (its
  `DllMain`'s `DLL_PROCESS_ATTACH`) completes the patch.
- `proxy/` — A thin "impersonation" DLL that forwards every export of
  `avfilter-12.dll` unchanged, and on load just `LoadLibrary`s
  `dda-hook-core` -- it holds no hook logic of its own. To target a
  different piece of software, write a new shim following this same pattern
  (an export-forwarding DLL impersonating whatever DLL that software loads)
  and it can reuse `dda-hook-core` as-is.
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
  本物の `IDXGIOutputDuplication` をダミーの実装（`DuplicationProxy`）で
  ラップして差し替えます。**専用のバックグラウンドスレッド（pump）は
  存在しません** — `AcquireNextFrame` は ddagrab 自身が呼んだその場で、
  同期的に本物のインスタンスへストレートに委譲します。
- 以前のバージョンは本物のインスタンスを裏側の専用スレッドで独自にポーリ
  ングし続ける設計（pump）でしたが、`hevc_nvenc` 等のエンコーダを繋いだ
  ときに ddagrab 自身のフレーム要求レートが 60Hz から 1〜2Hz まで落ち込む
  現象が確認され、原因が「同じ `ID3D11Device`/`ImmediateContext` に触る
  スレッドがもう1本存在すること自体」（pump が何をしているかではなく、
  存在することそのもの）にあると特定されたため撤去されました。詳細は
  [`dda-hook-core/src/hooks/duplication_proxy.rs`](dda-hook-core/src/hooks/duplication_proxy.rs)
  のモジュールコメントを参照してください。
- `ACCESS_LOST`/`ACCESS_DENIED`/`INVALID_CALL` を検知した場合は、
  `AcquireNextFrame` を呼んだそのスレッド上で同期的に復旧します。「古い
  インスタンスを先に drop してから再生成する」順序を守る必要があり
  （[`dda-hook-core/src/recovery.rs`](dda-hook-core/src/recovery.rs)）、
  復旧が完了するまでの間 ddagrab には `WAIT_TIMEOUT` だけを返すため、
  ddagrab 側からは常に「フレームがまだ来ていないだけ」にしか見えず、
  復旧処理そのものを意識しません。

- ddagrab's own calls to `DuplicateOutput`/`DuplicateOutput1` are hooked, and
  the real `IDXGIOutputDuplication` is wrapped by a stub implementation
  (`DuplicationProxy`). **There is no dedicated background "pump" thread** --
  `AcquireNextFrame` is a straight, synchronous passthrough to the real
  instance, called on whatever thread ddagrab itself calls from.
- An earlier version DID have a dedicated background thread polling the real
  instance independently (a "pump"). It was removed after diagnostics showed
  ddagrab's own frame-request rate dropping from 60Hz to as low as 1-2Hz once
  an encoder (`hevc_nvenc`) was downstream -- traced to the mere existence of
  a second thread continuously touching the same `ID3D11Device`/
  `ImmediateContext` ddagrab and NVENC also share (not anything the pump was
  doing per frame). See the module doc comment in
  [`dda-hook-core/src/hooks/duplication_proxy.rs`](dda-hook-core/src/hooks/duplication_proxy.rs)
  for the full story.
- On `ACCESS_LOST`/`ACCESS_DENIED`/`INVALID_CALL`, recovery runs inline, on
  that same calling thread: drop the dead instance FIRST, then re-duplicate
  -- that ordering turned out to be required for recovery to actually work
  ([`dda-hook-core/src/recovery.rs`](dda-hook-core/src/recovery.rs)). ddagrab
  only ever observes `WAIT_TIMEOUT` while this is in progress, so it only
  ever sees "no new frame yet" and never has to know recovery is happening
  at all.

## 他アプリとの互換性 / Compatibility with other applications

`dda-hook-core` は ddagrab や FFmpeg の関数・シンボルには一切依存していませ
ん。フックの起点は `d3d11.dll` がエクスポートするグローバル関数
`D3D11CreateDevice` へのインラインフック1つだけで（
[`dda-hook-core/src/hooks/d3d11_device.rs`](dda-hook-core/src/hooks/d3d11_device.rs)）、
そこから

`D3D11CreateDevice` → `QueryInterface`(`IDXGIDevice`) → `GetParent`(`IDXGIAdapter`)
→ `EnumOutputs`(`IDXGIOutput`) → `DuplicateOutput`/`DuplicateOutput1`
→ `AcquireNextFrame`

という COM vtable の連鎖を、実際にそのプロセスが呼び出した順に**動的に**追跡
してフックを仕込みます（`hooks::install_all()` が eager にインストールする
のは `D3D11CreateDevice` だけで、残りは初めて観測した時点で遅延インストール
されます — [`dda-hook-core/src/hooks/mod.rs`](dda-hook-core/src/hooks/mod.rs)）。
ffmpeg 固有の呼び出し規約や `vsrc_ddagrab.c` の内部実装に依存する処理は一切
ありません。

これはつまり、**Desktop Duplication API (`IDXGIOutputDuplication`) を
`D3D11CreateDevice` 経由で使うアプリケーションであれば、ffmpeg 以外でも
理論上そのまま動作するということです。** フックしている対象が

- `D3D11CreateDevice`（`d3d11.dll` の通常のエクスポート、静的インポートでも
  `LoadLibrary`+`GetProcAddress` 経由でも同じコードバイトを通るため区別なく
  捕捉できる）
- DXGI/D3D11 の COM vtable スロット（`IDXGIDevice`/`IDXGIAdapter`/
  `IDXGIOutput`/`IDXGIOutputDuplication` 系。Windows 側で ABI が固定されて
  いる公開インターフェース）

のみであり、呼び出し元アプリケーションの実装（OBS、ブラウザの画面共有、
独自のキャプチャツールなど）を一切問わないためです。

ただし実際に別アプリへ組み込むには、`proxy/` が行っているのと同じ手法
（対象アプリが読み込む DLL の全 export を forward しつつロード時に
`dda-hook-core` を `LoadLibrary` する、なりすまし DLL）を、対象アプリが
実際に読み込む DLL 名に合わせて別途用意する必要があります。`dda-hook-core`
自体はそのまま流用でき、ソースの変更は不要です。

- `dda-hook-core` has zero dependency on ddagrab or FFmpeg symbols. The only
  eagerly-installed hook is a single inline hook on the global function
  `D3D11CreateDevice` exported by `d3d11.dll`
  ([`dda-hook-core/src/hooks/d3d11_device.rs`](dda-hook-core/src/hooks/d3d11_device.rs)),
  from which it **dynamically** tracks the COM vtable chain

  `D3D11CreateDevice` → `QueryInterface`(`IDXGIDevice`) → `GetParent`(`IDXGIAdapter`)
  → `EnumOutputs`(`IDXGIOutput`) → `DuplicateOutput`/`DuplicateOutput1`
  → `AcquireNextFrame`

  in whatever order the host process actually calls it, lazily installing
  each downstream hook the first time that object is observed
  ([`dda-hook-core/src/hooks/mod.rs`](dda-hook-core/src/hooks/mod.rs)). None
  of this depends on ffmpeg's calling conventions or `vsrc_ddagrab.c`'s
  internals.

  In other words: **any application that uses the Desktop Duplication API
  (`IDXGIOutputDuplication`) via `D3D11CreateDevice` should theoretically work
  with this hook, not just ffmpeg.** The only things being hooked are

  - `D3D11CreateDevice` (an ordinary `d3d11.dll` export -- caught the same way
    whether the caller resolved it via static import or
    `LoadLibrary`+`GetProcAddress`, since both end up executing the same code
    bytes)
  - DXGI/D3D11 COM vtable slots (`IDXGIDevice`/`IDXGIAdapter`/`IDXGIOutput`/
    `IDXGIOutputDuplication` -- public interfaces with a stable Windows ABI)

  none of which care about the calling application's own implementation (OBS,
  a browser's screen-share, a custom capture tool, etc.).

  To actually deploy this against a different application, though, you'd need
  a new shim DLL following the same pattern `proxy/` uses (forward every
  export of whatever DLL that application loads, and `LoadLibrary` this
  `dda-hook-core` on attach), targeting that application's actual DLL name.
  `dda-hook-core` itself can be reused as-is, with no source changes.
