//! Legacy two-member-archive entrypoint. The embedded executables are data,
//! not linked TUI code. New installations use the separate split archive.
fn main() -> std::process::ExitCode {
    haider_compat::run(
        include_bytes!(env!("HAIDER_THIN_EXE")),
        include_bytes!(env!("HAIDER_TUI_EXE")),
    )
}
