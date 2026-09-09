//! Explicit temporary storage for the Android process; never mutate ART's env.
#[cfg(target_os = "android")]
static TEMP: ImmutableTempDirectory = ImmutableTempDirectory(std::sync::OnceLock::new());

#[cfg(any(test, target_os = "android"))]
#[derive(Default)]
struct ImmutableTempDirectory(std::sync::OnceLock<std::path::PathBuf>);

#[cfg(any(test, target_os = "android"))]
impl ImmutableTempDirectory {
    fn initialize(&self, path: &std::path::Path) -> std::io::Result<()> {
        self.initialize_with(path, || ())
    }

    fn initialize_with(
        &self,
        path: &std::path::Path,
        before_publish: impl FnOnce(),
    ) -> std::io::Result<()> {
        if path.canonicalize()? != path || !path.is_dir() {
            return Err(std::io::Error::other("native temp directory is invalid"));
        }
        if let Some(previous) = self.0.get() {
            return if previous == path {
                Ok(())
            } else {
                Err(std::io::Error::other("native temp directory is immutable"))
            };
        }
        before_publish();
        if let Err(candidate) = self.0.set(path.to_path_buf())
            && self.0.get().is_none_or(|installed| installed != &candidate)
        {
            return Err(std::io::Error::other("native temp directory is immutable"));
        }
        Ok(())
    }
}

#[cfg(target_os = "android")]
pub fn set_native_temp_directory(path: &std::path::Path) -> std::io::Result<()> {
    TEMP.initialize(path)
}

pub fn native_temp_directory() -> std::io::Result<std::path::PathBuf> {
    #[cfg(target_os = "android")]
    {
        TEMP.0
            .get()
            .cloned()
            .ok_or_else(|| std::io::Error::other("native temporary storage was not initialized"))
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(std::env::temp_dir())
    }
}

#[cfg(test)]
#[path = "native_temp_tests.rs"]
mod tests;
