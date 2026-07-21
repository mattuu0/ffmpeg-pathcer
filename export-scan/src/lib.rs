use anyhow::{Context, Result};
use pelite::pe64::{Pe, PeFile};
use std::path::Path;

/// Named exports of a PE DLL, in export-directory order.
pub fn read_named_exports(dll_path: &Path) -> Result<Vec<String>> {
    let map = pelite::FileMap::open(dll_path)
        .with_context(|| format!("failed to open {}", dll_path.display()))?;
    let file = PeFile::from_bytes(map.as_ref())
        .with_context(|| format!("failed to parse PE {}", dll_path.display()))?;
    let exports = file.exports().context("no export directory")?;
    let by = exports.by().context("failed to get export iterator")?;

    let mut names = Vec::new();
    for result in by.iter_names() {
        let (name_result, _rva_result) = result;
        if let Ok(name) = name_result {
            names.push(name.to_str()?.to_owned());
        }
    }
    Ok(names)
}
