use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_REAL_DLL: &str =
    "ffmpeg-master-latest-win64-lgpl-shared/bin/avfilter-12.dll";
const REAL_DLL_STEM: &str = "avfilter-12_orig";
const PROXY_DLL_STEM: &str = "avfilter-12";

fn main() {
    println!("cargo:rerun-if-env-changed=DDAGRAB_REAL_AVFILTER_DLL");
    println!("cargo:rerun-if-env-changed=DDAGRAB_LIB_EXE");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().unwrap().to_path_buf();

    let real_dll_path = env::var_os("DDAGRAB_REAL_AVFILTER_DLL")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join(DEFAULT_REAL_DLL));

    println!("cargo:rerun-if-changed={}", real_dll_path.display());

    if !real_dll_path.exists() {
        panic!(
            "real avfilter DLL not found at {}. Set DDAGRAB_REAL_AVFILTER_DLL to point at the \
             genuine avfilter-12.dll to forward exports to.",
            real_dll_path.display()
        );
    }

    let exports = export_scan::read_named_exports(&real_dll_path)
        .expect("failed to read exports of real avfilter DLL");

    if exports.is_empty() {
        panic!("real avfilter DLL exposed zero named exports; refusing to generate an empty .def");
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // 1. A plain (non-forwarding) .def naming the real DLL's own exports,
    //    used only to synthesize an import lib for it -- the forwarder
    //    syntax in our proxy's .def ("name = avfilter-12_orig.name") requires
    //    the linker be able to resolve symbols against *some* .lib for
    //    avfilter-12_orig.dll, which doesn't ship one (only the .dll does).
    let real_def_path = out_dir.join(format!("{REAL_DLL_STEM}.def"));
    write_plain_def(&real_def_path, REAL_DLL_STEM, &exports);

    let real_lib_path = out_dir.join(format!("{REAL_DLL_STEM}.lib"));
    generate_import_lib(&real_def_path, &real_lib_path, REAL_DLL_STEM);

    // 2. The proxy's own .def: every export forwards to the renamed real DLL.
    let proxy_def_path = out_dir.join(format!("{PROXY_DLL_STEM}.def"));
    write_forwarding_def(&proxy_def_path, PROXY_DLL_STEM, REAL_DLL_STEM, &exports);

    println!("cargo:rustc-link-arg=/DEF:{}", proxy_def_path.display());
    println!("cargo:rustc-link-arg={}", real_lib_path.display());

    println!(
        "cargo:warning=forwarding {} exports from {} to {}.dll (rename the real DLL to {}.dll before deploying)",
        exports.len(),
        real_dll_path.display(),
        REAL_DLL_STEM,
        REAL_DLL_STEM
    );
}

fn write_forwarding_def(def_path: &Path, proxy_stem: &str, real_stem: &str, exports: &[String]) {
    let mut out = String::new();
    out.push_str(&format!("LIBRARY {proxy_stem}\n"));
    out.push_str("EXPORTS\n");
    for name in exports {
        out.push_str(&format!("    {name} = {real_stem}.{name}\n"));
    }
    fs::write(def_path, out).expect("failed to write generated .def file");
}

fn write_plain_def(def_path: &Path, real_stem: &str, exports: &[String]) {
    let mut out = String::new();
    out.push_str(&format!("LIBRARY {real_stem}\n"));
    out.push_str("EXPORTS\n");
    for name in exports {
        out.push_str(&format!("    {name}\n"));
    }
    fs::write(def_path, out).expect("failed to write generated .def file");
}

fn generate_import_lib(def_path: &Path, lib_path: &Path, dll_stem: &str) {
    let lib_exe = env::var_os("DDAGRAB_LIB_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("lib.exe"));

    let status = Command::new(&lib_exe)
        .arg(format!("/DEF:{}", def_path.display()))
        .arg(format!("/OUT:{}", lib_path.display()))
        .arg("/MACHINE:X64")
        .arg(format!("/NAME:{dll_stem}.dll"))
        .current_dir(def_path.parent().unwrap())
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "failed to run {} to generate import lib for {dll_stem}.dll: {e}. \
                 Ensure the MSVC Build Tools 'lib.exe' is on PATH, or set DDAGRAB_LIB_EXE to its full path.",
                lib_exe.display()
            )
        });

    if !status.success() {
        panic!("lib.exe failed generating import lib for {dll_stem}.dll (exit {status})");
    }
}
