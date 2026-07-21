// ddagrab自身がDuplicateOutput/DuplicateOutput1で作るIDXGIOutputDuplicationを
// vtableフックで乗っ取り(hooks::install_all)、専用のpumpスレッド(hooks::pump)に
// 渡す。pumpは本物のAcquireNextFrame/ReleaseFrameを自分のペースで回し続け、
// ACCESS_LOST等が起きたら「古いインスタンスを先にdropしてから再複製する」順序を
// 守ってリカバリする(この順序がUAC/secure desktop遷移後も回復し続けるための鍵
// だったことを検証済み)。取得した各フレームはGPU上でキャッシュにコピーされ、
// ddagrab自身のAcquireNextFrame呼び出し(hooks::duplication_proxyのスタブ経由)
// はそのキャッシュの最新世代を返すだけで、本物のインスタンスには二度と触れない。
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
        crate::logging::plog!("ddagrab_proxy loaded, installing hooks");
        unsafe {
            hooks::install_all();
        }
    }
    TRUE
}
