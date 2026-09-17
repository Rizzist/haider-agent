//! Documents-convention tests (`docs/design/dated-workspace-v1.md` §2, W6).

use std::path::{Path, PathBuf};

use super::{
    DocumentsFallbackReason, DocumentsLookup, linux_documents_directory, parse_xdg_documents_dir,
};

fn home() -> PathBuf {
    PathBuf::from("/home/test-user")
}

#[test]
fn parses_home_relative_documents() {
    let parsed = parse_xdg_documents_dir("XDG_DOCUMENTS_DIR=\"$HOME/Documents\"\n", &home());
    assert_eq!(
        parsed,
        Some(DocumentsLookup::Documents(home().join("Documents")))
    );
}

#[test]
fn parses_absolute_and_unicode_and_spaces() {
    let parsed = parse_xdg_documents_dir(
        "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOCUMENTS_DIR=\"/mnt/data/Mes Documents é\"\n",
        &home(),
    );
    assert_eq!(
        parsed,
        Some(DocumentsLookup::Documents(PathBuf::from(
            "/mnt/data/Mes Documents é"
        )))
    );
}

#[test]
fn disabled_documents_falls_back_to_home_with_reason() {
    let parsed = parse_xdg_documents_dir("XDG_DOCUMENTS_DIR=\"$HOME\"\n", &home());
    assert_eq!(
        parsed,
        Some(DocumentsLookup::HomeFallback {
            home: home(),
            reason: DocumentsFallbackReason::Disabled,
        })
    );
}

#[test]
fn shell_syntax_is_data_never_executed() {
    for malicious in [
        "XDG_DOCUMENTS_DIR=\"$(rm -rf /)\"\n",
        "XDG_DOCUMENTS_DIR=\"`touch /tmp/pwn`\"\n",
        "XDG_DOCUMENTS_DIR=\"$HOME/$OTHER\"\n",
        "XDG_DOCUMENTS_DIR=\"relative/docs\"\n",
        "XDG_DOCUMENTS_DIR=unquoted\n",
        "XDG_DOCUMENTS_DIR=\"/a\\\"b\"\n",
    ] {
        let parsed = parse_xdg_documents_dir(malicious, &home());
        assert_eq!(
            parsed,
            Some(DocumentsLookup::HomeFallback {
                home: home(),
                reason: DocumentsFallbackReason::InvalidValue,
            }),
            "accepted malicious value: {malicious}"
        );
    }
}

#[test]
fn missing_file_or_key_is_lookup_unavailable() {
    let lookup = linux_documents_directory(&home(), None, |_| None);
    assert_eq!(
        lookup,
        DocumentsLookup::HomeFallback {
            home: home(),
            reason: DocumentsFallbackReason::LookupUnavailable,
        }
    );
    let lookup = linux_documents_directory(&home(), None, |_| {
        Some("XDG_DESKTOP_DIR=\"$HOME/Desktop\"\n".to_string())
    });
    assert_eq!(
        lookup,
        DocumentsLookup::HomeFallback {
            home: home(),
            reason: DocumentsFallbackReason::LookupUnavailable,
        }
    );
}

#[test]
fn relative_xdg_config_home_is_ignored_per_spec() {
    let mut asked = Vec::new();
    let asked_ref = std::cell::RefCell::new(&mut asked);
    let _ = linux_documents_directory(&home(), Some(Path::new("relative/config")), |path| {
        asked_ref.borrow_mut().push(path.to_path_buf());
        None
    });
    assert_eq!(asked, vec![home().join(".config/user-dirs.dirs")]);
}

#[test]
fn absolute_xdg_config_home_is_used() {
    let lookup = linux_documents_directory(&home(), Some(Path::new("/etc/xdg-home")), |path| {
        assert_eq!(path, Path::new("/etc/xdg-home/user-dirs.dirs"));
        Some("XDG_DOCUMENTS_DIR=\"$HOME/Docs\"\n".to_string())
    });
    assert_eq!(lookup, DocumentsLookup::Documents(home().join("Docs")));
}

#[test]
fn last_matching_line_wins_and_lookup_path_helper() {
    let content = "XDG_DOCUMENTS_DIR=\"$HOME/Old\"\nXDG_DOCUMENTS_DIR=\"$HOME/New\"\n".to_string();
    let parsed = parse_xdg_documents_dir(&content, &home()).unwrap();
    assert_eq!(parsed.path(), home().join("New").as_path());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_documents_is_home_documents() {
    let lookup = super::documents_directory(&home());
    assert_eq!(lookup, DocumentsLookup::Documents(home().join("Documents")));
}
