use anyhow::{bail, Result};
use std::path::PathBuf;

/// Prints the named exports of a DLL, one per line. Used to inspect what the
/// proxy DLL will need to forward, and by proxy/build.rs to generate the .def.
fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let Some(dll_path) = args.next() else {
        bail!("usage: xtask <path-to-dll>");
    };
    let dll_path = PathBuf::from(dll_path);

    let names = export_scan::read_named_exports(&dll_path)?;
    for name in &names {
        println!("{name}");
    }
    eprintln!("{} named exports", names.len());
    Ok(())
}
