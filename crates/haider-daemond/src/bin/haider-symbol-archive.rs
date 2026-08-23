//! Archives one release symbol companion under the executable's native ID.
//!
//! Usage: `haider-symbol-archive <binary> <symbols> <archive-root>`.
//! Cargo's packed split-debuginfo output is a dSYM bundle on macOS, a DWP on
//! ELF targets, and a PDB on MSVC. The archive key comes from the shipped
//! executable: Mach-O UUID, ELF build ID, or PE CodeView PDB GUID+age.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn main() {
    if let Err(error) = run(std::env::args_os().skip(1)) {
        eprintln!("haider-symbol-archive: {error}");
        std::process::exit(1);
    }
}

fn run(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    let binary = PathBuf::from(args.next().ok_or_else(|| usage("missing release binary"))?);
    let symbols = PathBuf::from(
        args.next()
            .ok_or_else(|| usage("missing symbol companion"))?,
    );
    let archive_root = PathBuf::from(args.next().ok_or_else(|| usage("missing archive root"))?);
    if args.next().is_some() {
        return Err(usage("too many arguments"));
    }
    if !binary.is_file() {
        return Err(format!(
            "release binary is not a file: {}",
            binary.display()
        ));
    }
    if !symbols.exists() {
        return Err(format!(
            "symbol companion does not exist: {}",
            symbols.display()
        ));
    }

    let identities = native_identities(&binary, &symbols)?;
    let symbol_name = symbols
        .file_name()
        .ok_or_else(|| format!("symbol companion has no file name: {}", symbols.display()))?;
    for identity in identities {
        let identity_root = archive_root.join(&identity);
        let destination = identity_root.join(symbol_name);
        if destination.exists() {
            return Err(format!(
                "refusing to overwrite existing symbol archive: {}",
                destination.display()
            ));
        }
        std::fs::create_dir_all(&identity_root)
            .map_err(|error| io_context("create identity archive", &identity_root, error))?;
        copy_tree(&symbols, &destination)
            .map_err(|error| io_context("copy symbol companion", &destination, error))?;
        let manifest = identity_root.join("identity.txt");
        let body = format!(
            "native_id={identity}\nbinary={}\nsymbols={}\n",
            binary.display(),
            symbol_name.to_string_lossy()
        );
        std::fs::write(&manifest, body)
            .map_err(|error| io_context("write identity manifest", &manifest, error))?;
        println!("native_id={identity} archived={}", destination.display());
    }
    Ok(())
}

fn usage(reason: &str) -> String {
    format!("{reason}; usage: haider-symbol-archive <binary> <symbols> <archive-root>")
}

fn io_context(action: &str, path: &Path, error: io::Error) -> String {
    format!("{action} {}: {error}", path.display())
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    if source.is_dir() {
        std::fs::create_dir(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        let mut input = std::fs::File::open(source)?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        io::copy(&mut input, &mut output)?;
    }
    Ok(())
}

fn command_output(program: &str, args: &[&OsStr]) -> Result<Output, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("run {program}: {error}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!(
            "{program} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(target_os = "macos")]
fn native_identities(binary: &Path, symbols: &Path) -> Result<BTreeSet<String>, String> {
    let binary_ids = macho_uuids(binary)?;
    let symbol_ids = macho_uuids(symbols)?;
    matching_macho_uuids(binary_ids, symbol_ids)
}

#[cfg(target_os = "macos")]
fn matching_macho_uuids(
    binary_ids: BTreeSet<String>,
    symbol_ids: BTreeSet<String>,
) -> Result<BTreeSet<String>, String> {
    if binary_ids != symbol_ids {
        return Err(format!(
            "Mach-O UUID mismatch: binary={binary_ids:?}, symbols={symbol_ids:?}"
        ));
    }
    Ok(binary_ids)
}

#[cfg(target_os = "macos")]
fn macho_uuids(path: &Path) -> Result<BTreeSet<String>, String> {
    let output = command_output(
        "xcrun",
        &[
            OsStr::new("dwarfdump"),
            OsStr::new("--uuid"),
            path.as_os_str(),
        ],
    )?;
    parse_macho_uuids(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(target_os = "macos")]
fn parse_macho_uuids(output: &str) -> Result<BTreeSet<String>, String> {
    let identities = output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("UUID: "))
        .filter_map(|line| line.split_whitespace().next())
        .filter(|uuid| uuid.len() == 36)
        .map(str::to_ascii_uppercase)
        .collect::<BTreeSet<_>>();
    if identities.is_empty() {
        Err("dwarfdump did not report a Mach-O UUID".into())
    } else {
        Ok(identities)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn native_identities(binary: &Path, _symbols: &Path) -> Result<BTreeSet<String>, String> {
    let output = command_output("readelf", &[OsStr::new("--notes"), binary.as_os_str()])?;
    let identity = parse_elf_build_id(&String::from_utf8_lossy(&output.stdout))?;
    Ok(BTreeSet::from([identity]))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn parse_elf_build_id(output: &str) -> Result<String, String> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Build ID:"))
        .map(str::trim)
        .filter(|identity| {
            !identity.is_empty() && identity.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| "readelf did not report an ELF build ID".into())
}

#[cfg(windows)]
fn native_identities(binary: &Path, _symbols: &Path) -> Result<BTreeSet<String>, String> {
    let llvm = command_output(
        "llvm-readobj",
        &[OsStr::new("--coff-debug-directory"), binary.as_os_str()],
    );
    let output = match llvm {
        Ok(output) => output,
        Err(llvm_error) => command_output("dumpbin", &[OsStr::new("/headers"), binary.as_os_str()])
            .map_err(|dumpbin_error| {
                format!("could not inspect PE CodeView identity ({llvm_error}; {dumpbin_error})")
            })?,
    };
    let identity = parse_pdb_key(&String::from_utf8_lossy(&output.stdout))?;
    Ok(BTreeSet::from([identity]))
}

#[cfg(any(windows, test))]
fn parse_pdb_key(output: &str) -> Result<String, String> {
    for line in output.lines() {
        let Some(rsds) = line.split_once("Format: RSDS,").map(|(_, rest)| rest) else {
            continue;
        };
        let mut fields = rsds.split(',').map(str::trim);
        let guid = fields.next().unwrap_or_default();
        let age = fields.next().unwrap_or_default();
        if let Some(key) = normalize_pdb_key(guid, age) {
            return Ok(key);
        }
    }

    let mut guid_bytes = None;
    let mut age = None;
    for line in output.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("PDBGUID:") {
            guid_bytes = parse_pdb_guid_bytes(value);
        }
        if let Some(value) = line.strip_prefix("PDBAge:") {
            age = Some(value.trim());
        }
    }
    guid_bytes
        .zip(age)
        .and_then(|(guid, age)| pdb_key_from_bytes(guid, age))
        .ok_or_else(|| "PE inspection did not report a PDB GUID+age".into())
}

#[cfg(any(windows, test))]
fn normalize_pdb_key(guid: &str, age: &str) -> Option<String> {
    let guid = guid
        .trim_matches(|character| matches!(character, '{' | '}'))
        .replace('-', "")
        .to_ascii_uppercase();
    if guid.len() != 32 || !guid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let age = age.strip_prefix("0x").map_or_else(
        || age.parse::<u32>().ok(),
        |hex| u32::from_str_radix(hex, 16).ok(),
    )?;
    Some(format!("{guid}{age:X}"))
}

#[cfg(any(windows, test))]
fn parse_pdb_guid_bytes(value: &str) -> Option<[u8; 16]> {
    let bytes = value
        .trim()
        .trim_matches(|character| matches!(character, '(' | ')'))
        .split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    bytes.try_into().ok()
}

#[cfg(any(windows, test))]
fn pdb_key_from_bytes(bytes: [u8; 16], age: &str) -> Option<String> {
    // CodeView stores GUID Data1/Data2/Data3 little-endian and Data4 in wire
    // order. Symbol servers use the canonical GUID text with separators
    // removed, followed by the age in hexadecimal.
    let order = [0, 1, 2, 3, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut canonical = bytes;
    canonical[..4].reverse();
    let mut guid = String::with_capacity(32);
    for index in order {
        use std::fmt::Write as _;
        write!(guid, "{:02X}", canonical[index]).ok()?;
    }
    let age = age.strip_prefix("0x").map_or_else(
        || age.parse::<u32>().ok(),
        |hex| u32::from_str_radix(hex, 16).ok(),
    )?;
    Some(format!("{guid}{age:X}"))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// MUTATION PIN (release symbols): change `split-debuginfo` to
    /// `"unpacked"` or `strip` to `"none"` in the workspace release profile.
    /// This fails before a release can silently ship without the packed
    /// companion, or regain the large embedded symbol payload.
    #[test]
    fn release_profile_packs_symbols_but_strips_the_shipped_image() {
        let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
        let manifest = std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", manifest_path.display()));
        let release = manifest
            .split_once("[profile.release]")
            .map(|(_, release)| release)
            .unwrap_or_else(|| panic!("{} has no release profile", manifest_path.display()));
        assert!(
            release
                .lines()
                .any(|line| line.trim() == "debug = \"line-tables-only\""),
            "release symbols need source line tables"
        );
        assert!(
            release
                .lines()
                .any(|line| line.trim() == "split-debuginfo = \"packed\""),
            "release symbols must be emitted as one archivable companion"
        );
        assert!(
            release
                .lines()
                .any(|line| line.trim() == "strip = \"symbols\""),
            "the distributed executable must stay stripped"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_every_macho_slice_uuid_as_an_archive_key() {
        let output = "UUID: 01234567-89ab-cdef-0123-456789abcdef (arm64) /tmp/haiderd\n\
                      UUID: fedcba98-7654-3210-fedc-ba9876543210 (x86_64) /tmp/haiderd\n";
        assert_eq!(
            parse_macho_uuids(output).expect("parse UUIDs"),
            BTreeSet::from([
                "01234567-89AB-CDEF-0123-456789ABCDEF".into(),
                "FEDCBA98-7654-3210-FEDC-BA9876543210".into(),
            ])
        );
    }

    /// MUTATION PIN (native symbol identity): remove the UUID inequality in
    /// `matching_macho_uuids`. A dSYM from another build is then accepted.
    #[cfg(target_os = "macos")]
    #[test]
    fn rejects_a_dsym_from_a_different_macho_build() {
        let binary = BTreeSet::from(["AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE".into()]);
        let symbols = BTreeSet::from(["11111111-2222-3333-4444-555555555555".into()]);
        let error = matching_macho_uuids(binary, symbols).expect_err("UUID mismatch must fail");
        assert!(error.starts_with("Mach-O UUID mismatch:"));
    }

    #[test]
    fn archive_refuses_to_overwrite_an_existing_identity() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "haider-symbol-archive-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("archive root");
        let source = root.join("haiderd.dwp");
        std::fs::write(&source, b"symbols").expect("write symbols");
        let destination = root.join("id").join("haiderd.dwp");
        std::fs::create_dir_all(destination.parent().expect("identity root"))
            .expect("create identity root");
        std::fs::write(&destination, b"old symbols").expect("write old archive");
        let error = copy_tree(&source, &destination).expect_err("must not overwrite");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        std::fs::remove_dir_all(&root).expect("remove archive root");
    }

    #[test]
    fn parses_llvm_pdb_guid_and_age_as_a_symbol_server_key() {
        let output = "PDBInfo {\n\
            PDBSignature: 0x53445352\n\
            PDBGUID: (96 83 40 42 81 07 9D 40 90 1B 4A 3C 0D 4F 56 32)\n\
            PDBAge: 3\n\
            PDBFileName: C:\\\\build\\\\haiderd.pdb\n\
        }\n";
        assert_eq!(
            parse_pdb_key(output).expect("parse PDB key"),
            "424083960781409D901B4A3C0D4F56323"
        );
    }
}
