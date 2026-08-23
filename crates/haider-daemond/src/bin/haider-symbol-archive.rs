//! Archives one release symbol companion under the executable's native ID.
//!
//! Usage: `haider-symbol-archive <binary> <symbols> <archive-root>`.
//! Cargo's packed split-debuginfo output is a dSYM bundle on macOS, a DWP on
//! ELF targets, and a PDB on MSVC. The archive key comes from the shipped
//! executable: Mach-O UUID, ELF build ID, or PE CodeView PDB GUID+age.

use std::collections::BTreeSet;
#[cfg(unix)]
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(unix)]
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

#[cfg(unix)]
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
    let identity = pe_pdb_key(binary)?;
    Ok(BTreeSet::from([identity]))
}

#[cfg(any(windows, test))]
fn pe_pdb_key(path: &Path) -> Result<String, String> {
    let image = std::fs::read(path).map_err(|error| io_context("read PE image", path, error))?;
    parse_pe_pdb_key(&image).map_err(|error| format!("inspect PE {}: {error}", path.display()))
}

#[cfg(any(windows, test))]
fn parse_pe_pdb_key(image: &[u8]) -> Result<String, String> {
    if image.get(..2) != Some(b"MZ") {
        return Err("missing DOS MZ signature".into());
    }

    let pe_offset = read_u32(image, 0x3c)
        .and_then(|offset| usize::try_from(offset).ok())
        .ok_or_else(|| "missing PE header offset".to_owned())?;
    if image.get(pe_offset..pe_offset.saturating_add(4)) != Some(b"PE\0\0") {
        return Err("missing PE signature".into());
    }

    let section_count = read_u16(image, pe_offset.saturating_add(6))
        .map(usize::from)
        .ok_or_else(|| "truncated COFF header".to_owned())?;
    let optional_size = read_u16(image, pe_offset.saturating_add(20))
        .map(usize::from)
        .ok_or_else(|| "truncated COFF header".to_owned())?;
    let optional_offset = pe_offset
        .checked_add(24)
        .ok_or_else(|| "PE header offset overflow".to_owned())?;
    let magic = read_u16(image, optional_offset)
        .ok_or_else(|| "truncated PE optional header".to_owned())?;
    let (directory_count_offset, directories_offset) = match magic {
        0x10b => (92usize, 96usize),
        0x20b => (108usize, 112usize),
        _ => {
            return Err(format!(
                "unsupported PE optional-header magic 0x{magic:04X}"
            ));
        }
    };
    let debug_directory_end = directories_offset
        .checked_add(7 * 8)
        .ok_or_else(|| "PE data-directory offset overflow".to_owned())?;
    if optional_size < debug_directory_end {
        return Err("PE optional header has no debug data directory".into());
    }
    let directory_count = read_u32(
        image,
        optional_offset.saturating_add(directory_count_offset),
    )
    .ok_or_else(|| "truncated PE data-directory count".to_owned())?;
    if directory_count <= 6 {
        return Err("PE optional header has no debug data directory".into());
    }

    let debug_entry_offset = optional_offset
        .checked_add(directories_offset + 6 * 8)
        .ok_or_else(|| "PE debug-directory offset overflow".to_owned())?;
    let debug_rva = read_u32(image, debug_entry_offset)
        .ok_or_else(|| "truncated PE debug data-directory entry".to_owned())?;
    let debug_size = read_u32(image, debug_entry_offset.saturating_add(4))
        .ok_or_else(|| "truncated PE debug data-directory entry".to_owned())?;
    if debug_rva == 0 || debug_size < 28 {
        return Err("PE debug data directory is empty".into());
    }

    let size_of_headers = read_u32(image, optional_offset.saturating_add(60))
        .ok_or_else(|| "truncated PE optional header".to_owned())?;
    let section_table_offset = optional_offset
        .checked_add(optional_size)
        .ok_or_else(|| "PE section-table offset overflow".to_owned())?;
    let debug_offset = pe_rva_to_file_offset(
        image,
        debug_rva,
        size_of_headers,
        section_table_offset,
        section_count,
    )
    .ok_or_else(|| format!("PE debug-directory RVA 0x{debug_rva:X} is not file-backed"))?;
    let entry_count = usize::try_from(debug_size / 28)
        .map_err(|_| "PE debug-directory size does not fit this host".to_owned())?;

    for index in 0..entry_count {
        let entry_offset = index
            .checked_mul(28)
            .and_then(|delta| debug_offset.checked_add(delta))
            .ok_or_else(|| "PE debug-directory entry offset overflow".to_owned())?;
        let debug_type = read_u32(image, entry_offset.saturating_add(12))
            .ok_or_else(|| "truncated PE debug-directory entry".to_owned())?;
        if debug_type != 2 {
            continue;
        }

        let data_size = read_u32(image, entry_offset.saturating_add(16))
            .ok_or_else(|| "truncated PE CodeView entry".to_owned())?;
        if data_size < 24 {
            continue;
        }
        let data_rva = read_u32(image, entry_offset.saturating_add(20))
            .ok_or_else(|| "truncated PE CodeView entry".to_owned())?;
        let data_pointer = read_u32(image, entry_offset.saturating_add(24))
            .ok_or_else(|| "truncated PE CodeView entry".to_owned())?;
        let data_offset = if data_pointer == 0 {
            pe_rva_to_file_offset(
                image,
                data_rva,
                size_of_headers,
                section_table_offset,
                section_count,
            )
        } else {
            usize::try_from(data_pointer).ok()
        };
        let Some(data_offset) = data_offset else {
            continue;
        };
        let Some(signature) = image.get(data_offset..data_offset.saturating_add(4)) else {
            continue;
        };
        if signature != b"RSDS" {
            continue;
        }
        let Some(guid) = image.get(data_offset.saturating_add(4)..data_offset.saturating_add(20))
        else {
            continue;
        };
        let Some(age) = read_u32(image, data_offset.saturating_add(20)) else {
            continue;
        };
        let guid: [u8; 16] = guid
            .try_into()
            .map_err(|_| "invalid PE CodeView GUID size".to_owned())?;
        return Ok(pdb_key_from_bytes(guid, age));
    }

    Err("PE debug directory has no RSDS CodeView PDB GUID+age".into())
}

#[cfg(any(windows, test))]
fn pe_rva_to_file_offset(
    image: &[u8],
    rva: u32,
    size_of_headers: u32,
    section_table_offset: usize,
    section_count: usize,
) -> Option<usize> {
    if rva < size_of_headers {
        let offset = usize::try_from(rva).ok()?;
        return (offset < image.len()).then_some(offset);
    }

    for index in 0..section_count {
        let section_offset = index
            .checked_mul(40)
            .and_then(|delta| section_table_offset.checked_add(delta))?;
        let virtual_size = read_u32(image, section_offset.checked_add(8)?)?;
        let virtual_address = read_u32(image, section_offset.checked_add(12)?)?;
        let raw_size = read_u32(image, section_offset.checked_add(16)?)?;
        let raw_pointer = read_u32(image, section_offset.checked_add(20)?)?;
        let Some(delta) = rva.checked_sub(virtual_address) else {
            continue;
        };
        if delta >= virtual_size.max(raw_size) || delta >= raw_size {
            continue;
        }
        let offset = raw_pointer
            .checked_add(delta)
            .and_then(|value| usize::try_from(value).ok())?;
        if offset < image.len() {
            return Some(offset);
        }
    }
    None
}

#[cfg(any(windows, test))]
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value: [u8; 2] = bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(value))
}

#[cfg(any(windows, test))]
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(value))
}

#[cfg(any(windows, test))]
fn pdb_key_from_bytes(bytes: [u8; 16], age: u32) -> String {
    // CodeView stores GUID Data1/Data2/Data3 little-endian and Data4 in wire
    // order. Symbol servers use the canonical GUID text with separators
    // removed, followed by the age in hexadecimal.
    format!(
        "{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{age:X}",
        bytes[3],
        bytes[2],
        bytes[1],
        bytes[0],
        bytes[5],
        bytes[4],
        bytes[7],
        bytes[6],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
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

    /// A minimal PE32+ image with one file-backed `.rdata` section. Its debug
    /// data directory points at an IMAGE_DEBUG_TYPE_CODEVIEW entry and an RSDS
    /// record, exactly as an MSVC-linked release executable does.
    fn pe_codeview_fixture() -> Vec<u8> {
        fn put_u16(image: &mut [u8], offset: usize, value: u16) {
            image[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }

        fn put_u32(image: &mut [u8], offset: usize, value: u32) {
            image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }

        let mut image = vec![0u8; 0x400];
        image[..2].copy_from_slice(b"MZ");
        put_u32(&mut image, 0x3c, 0x80);
        image[0x80..0x84].copy_from_slice(b"PE\0\0");

        // COFF header: AMD64, one section, and a 240-byte PE32+ optional header.
        put_u16(&mut image, 0x84, 0x8664);
        put_u16(&mut image, 0x86, 1);
        put_u16(&mut image, 0x94, 0x00f0);
        put_u16(&mut image, 0x96, 0x0022);

        let optional = 0x98;
        put_u16(&mut image, optional, 0x020b);
        put_u32(&mut image, optional + 32, 0x1000); // SectionAlignment
        put_u32(&mut image, optional + 36, 0x0200); // FileAlignment
        put_u32(&mut image, optional + 56, 0x2000); // SizeOfImage
        put_u32(&mut image, optional + 60, 0x0200); // SizeOfHeaders
        put_u16(&mut image, optional + 68, 3); // Console subsystem
        put_u32(&mut image, optional + 108, 16); // NumberOfRvaAndSizes
        let debug_data_directory = optional + 112 + 6 * 8;
        put_u32(&mut image, debug_data_directory, 0x1000);
        put_u32(&mut image, debug_data_directory + 4, 28);

        let section = optional + 0x00f0;
        image[section..section + 8].copy_from_slice(b".rdata\0\0");
        put_u32(&mut image, section + 8, 0x0200);
        put_u32(&mut image, section + 12, 0x1000);
        put_u32(&mut image, section + 16, 0x0200);
        put_u32(&mut image, section + 20, 0x0200);
        put_u32(&mut image, section + 36, 0x4000_0040);

        // IMAGE_DEBUG_DIRECTORY followed by its RSDS record at file offset 0x220.
        put_u32(&mut image, 0x20c, 2); // IMAGE_DEBUG_TYPE_CODEVIEW
        put_u32(&mut image, 0x210, 0x31);
        put_u32(&mut image, 0x214, 0x1020);
        put_u32(&mut image, 0x218, 0x0220);
        image[0x220..0x224].copy_from_slice(b"RSDS");
        image[0x224..0x234].copy_from_slice(&[
            0xCD, 0x49, 0xE4, 0x8C, 0x41, 0x75, 0x97, 0x60, 0x4C, 0x4C, 0x44, 0x20, 0x50, 0x44,
            0x42, 0x2E,
        ]);
        put_u32(&mut image, 0x234, 0x1A);
        image[0x238..0x251].copy_from_slice(b"haider-profile-probe.pdb\0");
        image
    }

    /// MUTATION PIN (PE symbol identity): change the CodeView type at fixture
    /// offset 0x20c from 2 to 1. The real PE reader must then reject the image
    /// because the RSDS payload is no longer identified as CodeView data.
    #[test]
    fn reads_pdb_guid_and_age_from_a_fixture_pe_binary() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "haider-symbol-archive-pe-fixture-{}-{nonce}.exe",
            std::process::id()
        ));
        std::fs::write(&path, pe_codeview_fixture()).expect("write fixture PE");
        assert_eq!(
            pe_pdb_key(&path).expect("read PDB key from fixture PE"),
            "8CE449CD754160974C4C44205044422E1A"
        );
        std::fs::remove_file(path).expect("remove fixture PE");
    }
}
