// ddagrab自身がDuplicateOutput/DuplicateOutput1で作るIDXGIOutputDuplicationを
// vtableフックで乗っ取り(hooks::install_all)、hooks::duplication_proxyのラッパー
// に渡す。ddagrab自身のAcquireNextFrame/ReleaseFrame呼び出しは、専用スレッドを
// 挟まず本物のインスタンスへそのまま素通しする(常駐スレッドを持たせるとNVENC等
// 下流のGPU処理と競合し、ddagrab側のフレーム要求頻度が時間とともに低下する現象を
// 確認したため、素通し方式に変更した)。ACCESS_LOST等が起きた場合のみ、ddagrabの
// 呼び出しスレッド上でその場で同期的にリカバリする -- 「古いインスタンスを先に
// dropしてから再複製する」順序を守る点は変わらない(この順序がUAC/secure desktop
// 遷移後も回復し続けるための鍵だったことを検証済み)。ddagrab自身はWAIT_TIMEOUT
// かOkしか観測せず、ACCESS_LOST自体を見ることは無い。
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
