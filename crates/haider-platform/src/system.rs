/// Returns the platform's short local host name in lowercase.
///
/// Unix deliberately retains the historical `uname(2)` implementation.
#[cfg(unix)]
#[must_use]
pub fn local_device_name() -> Option<String> {
    let uname = rustix::system::uname();
    let node = uname.nodename().to_string_lossy();
    let short = node.split('.').next().unwrap_or("").trim().to_lowercase();
    (!short.is_empty()).then_some(short)
}

#[cfg(windows)]
#[must_use]
pub fn local_device_name() -> Option<String> {
    std::env::var_os("COMPUTERNAME")
        .and_then(|name| name.into_string().ok())
        .and_then(|name| {
            let short = name
                .split('.')
                .next()
                .unwrap_or("")
                .trim()
                .to_lowercase();
            (!short.is_empty()).then_some(short)
        })
}
