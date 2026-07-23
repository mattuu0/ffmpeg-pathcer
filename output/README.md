# ddagrab_proxy (avfilter-12.dll) の使い方 / Usage

FFmpeg の `ddagrab`（Desktop Duplication API）フィルタが、UAC/secure desktop
(Winlogon) への画面遷移をまたいでもキャプチャを継続できるようにするパッチ
DLL です。本物の `avfilter-12.dll` の全 export をそのまま転送しつつ、DXGI の
Desktop Duplication 呼び出しだけを裏側でフックして復旧処理を行います。

A proxy DLL that patches FFmpeg's `ddagrab` (Desktop Duplication API) filter
so capture keeps running across UAC / secure desktop (Winlogon) transitions.
It forwards every export of the real `avfilter-12.dll` unchanged, and only
hooks the DXGI Desktop Duplication calls behind the scenes to recover.

---

## ⚠️ 免責事項 / DISCLAIMER

**本ソフトウェアは無保証（AS IS）で提供されます。使用によって何が起きても、
作者・貢献者・配布者は一切の責任を負いません。** 利用するかどうか、利用に
よって生じた結果はすべて利用者自身の責任です。

**このソフトウェアに、認証回避・権限昇格・自己増殖・隠蔽機能などの危険な
コードは含まれていません。** 使っているのは公開の Windows API
（DXGI/Direct3D11）と、通常の DLL エクスポート転送だけです。

**悪意のある使用は厳禁です。** 具体的には、相手の同意なく端末に無断でインス
トールすること、このソフトが何をするか偽って配布・使用させること、不正ア
クセスや盗撮・ストーカー行為など法令に触れる目的での使用はできません。リ
モートデスクトップ／リモートサポートツールなど、正当な同意に基づいて他者
の端末を録画・監視する用途への組み込みは問題ありません。

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

---

## これは何をするものか / What it does

- UAC の同意ダイアログ (secure desktop) が出ている間、通常は `ddagrab` の
  `AcquireNextFrame` が `ACCESS_LOST` 等で失敗し、キャプチャが止まってしまい
  ます。
- このプロキシは ddagrab が握る `IDXGIOutputDuplication` を裏側の専用スレッド
  (pump) に肩代わりさせ、UAC 遷移中も裏側で自動的に再取得(recover)し続ける
  ことで、ddagrab 側からは「フレームがまだ来ていないだけ」に見えるようにし
  ます。
- マウスカーソルの描画 (`draw_mouse`、デフォルト有効) にも対応済みです。

Normally, while the UAC consent dialog (secure desktop) is shown, ddagrab's
own `AcquireNextFrame` fails with `ACCESS_LOST` and capture stops. This proxy
hands ddagrab's `IDXGIOutputDuplication` off to a dedicated background
thread ("pump") that keeps re-acquiring it on its own across the transition,
so ddagrab itself only ever sees "no new frame yet". Mouse cursor rendering
(`draw_mouse`, on by default) is supported as well.

## インストール方法 / Installation

1. `ddagrab_proxy.dll` と `dda_hook_core.dll` を、使っている FFmpeg 配布物の
   `bin/avfilter-12.dll` と同じフォルダに置きます（両方必要です — フック本体
   は `dda_hook_core.dll` 側にあり、`ddagrab_proxy.dll` はロード時にそれを
   読み込むだけです）。
2. **本物の `avfilter-12.dll` を `avfilter-12_orig.dll` にリネーム**します
   （このプロキシは実体の DLL を全 export フォワーディングで包んでいるだけ
   なので、本物がこの名前で存在しないと起動時に失敗します）。
3. `ddagrab_proxy.dll` を `avfilter-12.dll` という名前でコピー（またはリネ
   ーム）して配置します。

1. Place both `ddagrab_proxy.dll` and `dda_hook_core.dll` in the same folder
   as your FFmpeg build's `bin/avfilter-12.dll` (both are required — the hook
   implementation lives in `dda_hook_core.dll`; `ddagrab_proxy.dll` just loads
   it on attach).
2. **Rename the real `avfilter-12.dll` to `avfilter-12_orig.dll`** (the proxy
   forwards every export to the DLL under that exact name, so it will fail
   to load if the real one isn't there).
3. Copy (or rename) `ddagrab_proxy.dll` into place as `avfilter-12.dll`.

## アンインストール方法 / Uninstallation

1. `bin/avfilter-12.dll`（プロキシ）と `bin/dda_hook_core.dll` を削除。
2. `bin/avfilter-12_orig.dll` を `avfilter-12.dll` にリネームし直す。

1. Delete `bin/avfilter-12.dll` (the proxy) and `bin/dda_hook_core.dll`.
2. Rename `bin/avfilter-12_orig.dll` back to `avfilter-12.dll`.

## 使い方 / Usage

インストール後は、通常通り `ddagrab` フィルタを使った ffmpeg コマンドを実行
するだけです。特別なオプションは不要です。

Once installed, just run ffmpeg with the `ddagrab` filter as usual -- no
special options required.

```
ffmpeg -f lavfi -i ddagrab=output_idx=0 -c:v h264_nvenc -y out.mp4
```

`output_idx` でモニターを指定するオプションもそのまま使えます（複数モニタ
構成でも UAC 復旧時に元と同じモニタを再取得するよう対応済みです）。

The `output_idx` option for selecting a monitor works as-is (multi-monitor
setups are handled correctly -- recovery re-acquires the same monitor you
started with).

## 配布物の検証（SHA256） / Verifying the download (SHA256)

配布 zip には DLL 本体のハッシュ値を記載したテキストファイル
（`SHA256SUMS.txt`）を同梱しています。ダウンロードした DLL が改ざんされて
いないか、PowerShell で以下のように確認できます。

The distribution zip includes a `SHA256SUMS.txt` with the DLL's hash. Verify
your download with PowerShell:

```powershell
Get-FileHash .\ddagrab_proxy.dll -Algorithm SHA256
```

出力された `Hash` の値が `SHA256SUMS.txt` に記載の値と一致することを確認
してください。一致しない場合はファイルが破損しているか改ざんされている
可能性があるため、使用しないでください。

Confirm the printed `Hash` matches the value in `SHA256SUMS.txt`. If it does
not match, the file may be corrupted or tampered with -- do not use it.

## ログ / Logging

同じフォルダに `ddagrab_proxy.log`（プロキシ自身のロード状況）と
`dda_hook_core.log`（フック状況・UAC/secure desktop 遷移の検知・復旧の成否
など）の 2 つが生成されます。問題が起きた場合はまず後者の末尾（`dda_hook_core
loaded` 以降の一番新しい実行分）を確認してください。どちらも起動のたびに
追記されるだけで自動ローテートはされないため、肥大化が気になる場合は適宜
削除してください。

Two log files are created in the same folder: `ddagrab_proxy.log` (the
proxy's own load status) and `dda_hook_core.log` (hook installation, desktop
transitions, and recovery success/failure). If something goes wrong, check
the tail of the latter (after the most recent `dda_hook_core loaded` line)
first. Neither is rotated automatically -- delete them yourself if they grow
too large.

## 制限事項・注意点 / Limitations

- **FFmpeg 本体のバージョンアップには基本的に追従できます** — export の一覧
  はビルド時に実際の DLL をスキャンして自動生成しているため、`ddagrab` 以外
  の関数が増減してもソース変更なしで再ビルドすれば対応できます。
- **ただし `libavfilter` のメジャーバージョンが変わる（DLL 名が
  `avfilter-12.dll` から `avfilter-13.dll` 等に変わる）場合は対応できませ
  ん**。その場合は本体側のビルド設定（DLL 名の定数）を更新して再ビルドする
  必要があります。
- フックしているのは DXGI/D3D11 の COM インターフェース（Windows 側の安定
  した ABI）なので、ffmpeg のマイナー/パッチバージョン更新による影響は基本
  的に受けません。
- `vsrc_ddagrab.c` 側の内部実装（呼び出し順序やデフォルト値など）が将来的に
  大きく変わった場合、キャプチャ自体は動いても一部の挙動（カーソル描画等）
  がずれる可能性があります。
- 32bit (x86) ffmpeg には対応していません（x64 前提でビルドされています）。

- **Generally tolerant of FFmpeg version bumps** -- the export list is
  scanned from the real DLL at build time, so a rebuild picks up any change
  automatically, no source changes needed.
- **Not tolerant of a `libavfilter` SONAME/major-version bump** (e.g.
  `avfilter-12.dll` becoming `avfilter-13.dll`) -- that requires updating the
  build configuration and rebuilding.
- The hooked surface (DXGI/D3D11 COM interfaces) is a stable Windows ABI, so
  ffmpeg minor/patch updates generally don't affect it.
- If `vsrc_ddagrab.c`'s internals (call order, defaults, etc.) change
  significantly upstream, capture may keep working while some behavior
  (e.g. cursor rendering) drifts from what's documented here.
- 32-bit (x86) ffmpeg is not supported (this is built x64-only; see the
  `arm64` build for Windows on ARM).
