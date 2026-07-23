// このDLL自体はavfilter-12.dllの全exportをforwardする「なりすまし」殻でしかなく、
// フック/リカバリ実装は一切持たない(実体はdda-hook-core crateに切り出し済み)。
// ここでやることはただ一つ: 自分がプロセスにロードされたら、同じディレクトリに
// 置かれているdda_hook_core.dllをLoadLibraryで読み込むこと。dda_hook_core.dll
// 自身のDllMainがDLL_PROCESS_ATTACHでhooks::install_all()相当を実行し、それだけで
// パッチが完了する(このDLLからフックを呼び出す必要はない)。
//
// 実行中プロセスへの外部注入(CreateRemoteThread+LoadLibrary、SetWindowsHookEx、
// AppInit_DLLs等)は一切行わない -- ここでのLoadLibraryは、対象アプリ自身が
// 起動時に静的にこのDLL(を名乗る本体)を読み込んだ、その延長で実行されるだけ。
mod logging;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, TRUE};
use windows::Win32::System::LibraryLoader::LoadLibraryW;
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

const CORE_DLL_NAME: &str = "dda_hook_core.dll";

#[unsafe(no_mangle)]
#[allow(non_snake_case, unused_variables)]
extern "system" fn DllMain(module: HINSTANCE, reason: u32, reserved: *mut core::ffi::c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        logging::init();
        crate::logging::plog!("ddagrab_proxy loaded, loading {CORE_DLL_NAME}");

        let wide: Vec<u16> = CORE_DLL_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        match unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) } {
            Ok(_) => crate::logging::plog!("{CORE_DLL_NAME} loaded successfully"),
            Err(e) => crate::logging::plog!("failed to load {CORE_DLL_NAME}: {e:?}"),
        }
    }
    TRUE
}
