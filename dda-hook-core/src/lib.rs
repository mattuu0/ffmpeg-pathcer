// DXGI Desktop Duplication (IDXGIOutputDuplication) を使う任意のプロセスに対する
// 汎用フック本体。FFmpeg/ddagrabに依存しない -- ddagrab_proxy(旧: 単一crate)から
// フック/リカバリ実装だけを切り出したもの。このDLLがプロセスにロードされ、
// DllMainがDLL_PROCESS_ATTACHを受け取った時点でhooks::install_all()が走り、
// それだけでパッチが完了する(呼び出し元は他に何もする必要がない)。
//
// D3D11CreateDeviceのインラインフックを起点に、QueryInterface -> GetParent(Adapter)
// -> EnumOutputs -> DuplicateOutput(1) -> AcquireNextFrameの連鎖を自動追跡し、
// ACCESS_LOST等が起きた場合のみ、呼び出し元スレッド上でその場で同期的にリカバリする。
// 詳細はrecovery.rs / hooks::duplication_proxyのモジュールコメントを参照。
mod hooks;
mod logging;
mod recovery;
mod state;

use windows::core::BOOL;
use windows::Win32::Foundation::{HINSTANCE, TRUE};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

#[unsafe(no_mangle)]
#[allow(non_snake_case, unused_variables)]
extern "system" fn DllMain(module: HINSTANCE, reason: u32, reserved: *mut core::ffi::c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        logging::init();
        crate::logging::plog!("dda_hook_core loaded, installing hooks");
        unsafe {
            hooks::install_all();
        }
    }
    TRUE
}
