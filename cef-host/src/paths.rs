use std::path::PathBuf;
use std::sync::OnceLock;

static CEF_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn init(cef_dir: PathBuf) {
    let _ = CEF_DIR.set(cef_dir);
}

/// `<game>/cef`: where libcef.dll, renderer.exe and this executable live.
pub fn cef_dir() -> PathBuf {
    CEF_DIR
        .get()
        .cloned()
        .or_else(|| std::env::current_exe().ok()?.parent().map(PathBuf::from))
        .unwrap_or_default()
}

/// Folder with cef_host.exe, renderer.exe and the 64-bit CEF runtime (`cef/host`). It is
/// separate from `cef/` because gta_sa.exe loads the old 32-bit libcef.dll from there.
pub fn runtime_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .unwrap_or_else(cef_dir)
}

pub fn assets_dir() -> PathBuf {
    cef_dir().join("assets")
}
