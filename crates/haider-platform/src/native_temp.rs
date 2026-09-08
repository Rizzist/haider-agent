//! Explicit temporary storage for the Android process; never mutate ART's env.
#[cfg(target_os = "android")]
static TEMP: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

#[cfg(target_os = "android")]
pub fn set_native_temp_directory(path: &std::path::Path) -> std::io::Result<()> {
    if path.canonicalize()? != path || !path.is_dir() {
        return Err(std::io::Error::other("native temp directory is invalid"));
    }
    if let Some(previous) = TEMP.get() {
        return if previous == path {
            Ok(())
        } else {
            Err(std::io::Error::other("native temp directory is immutable"))
        };
    }
    TEMP.set(path.to_path_buf())
        .map_err(|_| std::io::Error::other("native temp directory is already set"))
}

pub fn native_temp_directory() -> std::io::Result<std::path::PathBuf> {
    #[cfg(target_os = "android")]
    {
        TEMP.get()
            .cloned()
            .ok_or_else(|| std::io::Error::other("native temporary storage was not initialized"))
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(std::env::temp_dir())
    }
}
