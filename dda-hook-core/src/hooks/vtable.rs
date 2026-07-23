use std::collections::HashSet;
use std::ffi::c_void;

use parking_lot::Mutex;
use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};

/// (vtable pointer, slot index) pairs we've already patched, so repeated
/// QueryInterface/EnumOutputs calls on objects backed by the same concrete
/// class don't re-patch (COM vtables are per-class, shared across every
/// instance of that class).
///
/// Keyed by (vtable, slot) rather than just vtable: a single concrete DXGI
/// class commonly implements the entire IDXGIOutput -> IDXGIOutput6 chain
/// (and similarly for IDXGIDevice/IDXGIDevice4, etc.) through ONE shared
/// vtable, so QueryInterface for a "different" interface on the same object
/// can return the very same vtable pointer already recorded for an earlier,
/// unrelated slot. Deduping on vtable alone would then silently skip
/// installing a hook on a slot that was never actually patched.
static PATCHED: Mutex<Option<HashSet<(usize, usize)>>> = Mutex::new(None);

pub fn already_patched(vtable_ptr: *mut *mut c_void, slot_index: usize) -> bool {
    let mut guard = PATCHED.lock();
    let set = guard.get_or_insert_with(HashSet::new);
    !set.insert((vtable_ptr as usize, slot_index))
}

/// Overwrites a single vtable slot, returning the original function pointer.
///
/// # Safety
/// `vtable_ptr` must point at a valid, live COM vtable with at least
/// `slot_index + 1` entries.
pub unsafe fn patch_slot(
    vtable_ptr: *mut *mut c_void,
    slot_index: usize,
    new_fn: *mut c_void,
) -> *mut c_void {
    let slot_addr = vtable_ptr.add(slot_index);
    let mut old_protect = PAGE_PROTECTION_FLAGS(0);
    VirtualProtect(
        slot_addr as *const c_void,
        size_of::<*mut c_void>(),
        PAGE_EXECUTE_READWRITE,
        &mut old_protect,
    )
    .expect("VirtualProtect (unprotect vtable slot) failed");

    let original = *slot_addr;
    *slot_addr = new_fn;

    let mut ignored = PAGE_PROTECTION_FLAGS(0);
    VirtualProtect(
        slot_addr as *const c_void,
        size_of::<*mut c_void>(),
        old_protect,
        &mut ignored,
    )
    .expect("VirtualProtect (restore vtable slot) failed");

    original
}

/// A minimal x86-64 inline hook: overwrites the first 12 bytes of `target`
/// with `mov rax, imm64; jmp rax` to `detour`, saving the original bytes so
/// the trampoline can be restored/called through.
///
/// Used to hook plain exported functions (e.g. `D3D11CreateDevice`) rather
/// than COM vtable slots -- simpler and more robust than EAT-directory
/// patching, and works regardless of whether the caller resolved the address
/// via static import or `LoadLibrary`+`GetProcAddress` (both end up calling
/// through the same code bytes).
pub struct InlineHook {
    target: *mut u8,
    original_bytes: [u8; 12],
}

impl InlineHook {
    /// # Safety
    /// `target` must point at an executable function with at least 12 bytes
    /// of code that can be safely clobbered (true for ordinary non-leaf-tiny
    /// system DLL exports like `D3D11CreateDevice`), and must not currently
    /// be executing on another thread while this runs.
    pub unsafe fn install(target: *mut c_void, detour: *mut c_void) -> InlineHook {
        let target = target as *mut u8;

        let mut original_bytes = [0u8; 12];
        std::ptr::copy_nonoverlapping(target, original_bytes.as_mut_ptr(), 12);

        let mut stub = [0u8; 12];
        stub[0] = 0x48; // REX.W
        stub[1] = 0xB8; // mov rax, imm64
        stub[2..10].copy_from_slice(&(detour as u64).to_le_bytes());
        stub[10] = 0xFF; // jmp rax
        stub[11] = 0xE0;

        let mut old_protect = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(target as *const c_void, 12, PAGE_EXECUTE_READWRITE, &mut old_protect)
            .expect("VirtualProtect (unprotect hook target) failed");

        std::ptr::copy_nonoverlapping(stub.as_ptr(), target, 12);

        let mut ignored = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(target as *const c_void, 12, old_protect, &mut ignored)
            .expect("VirtualProtect (restore hook target protection) failed");

        InlineHook { target, original_bytes }
    }

    /// Temporarily restores the original bytes, useful for calling straight
    /// into the un-hooked implementation from within the detour itself.
    ///
    /// # Safety
    /// Must not race with another thread entering `target` mid-restore.
    pub unsafe fn call_through<F: FnOnce() -> R, R>(&self, f: F) -> R {
        let mut saved = [0u8; 12];
        std::ptr::copy_nonoverlapping(self.target, saved.as_mut_ptr(), 12);

        let mut old_protect = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(self.target as *const c_void, 12, PAGE_EXECUTE_READWRITE, &mut old_protect)
            .expect("VirtualProtect failed");
        std::ptr::copy_nonoverlapping(self.original_bytes.as_ptr(), self.target, 12);
        let mut ignored = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(self.target as *const c_void, 12, old_protect, &mut ignored)
            .expect("VirtualProtect failed");

        let result = f();

        let mut old_protect2 = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(self.target as *const c_void, 12, PAGE_EXECUTE_READWRITE, &mut old_protect2)
            .expect("VirtualProtect failed");
        std::ptr::copy_nonoverlapping(saved.as_ptr(), self.target, 12);
        let mut ignored2 = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(self.target as *const c_void, 12, old_protect2, &mut ignored2)
            .expect("VirtualProtect failed");

        result
    }
}

unsafe impl Send for InlineHook {}
unsafe impl Sync for InlineHook {}
