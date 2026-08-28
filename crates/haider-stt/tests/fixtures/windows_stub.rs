//! Standalone native fixture compiled by the Windows integration tests.

use std::io::Write as _;
use std::path::PathBuf;

struct Decoder {
    bytes: Vec<u8>,
    offset: usize,
}

impl Decoder {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, offset: 0 }
    }

    fn byte(&mut self) -> Result<u8, String> {
        let byte = self
            .bytes
            .get(self.offset)
            .copied()
            .ok_or_else(|| "fixture config ended early".to_owned())?;
        self.offset += 1;
        Ok(byte)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or_else(|| "fixture config length overflowed".to_owned())?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| "fixture config ended early".to_owned())?;
        self.offset = end;
        bytes
            .try_into()
            .map_err(|_| "fixture config field had the wrong width".to_owned())
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.fixed()?))
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.fixed()?))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.fixed()?))
    }

    fn blob(&mut self) -> Result<Vec<u8>, String> {
        let len = self.u32()? as usize;
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| "fixture blob length overflowed".to_owned())?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| "fixture blob ended early".to_owned())?
            .to_vec();
        self.offset = end;
        Ok(bytes)
    }

    fn string(&mut self) -> Result<String, String> {
        String::from_utf8(self.blob()?).map_err(|_| "fixture string was not UTF-8".to_owned())
    }
}

fn write_stdout(bytes: &[u8]) -> Result<(), String> {
    std::io::stdout()
        .write_all(bytes)
        .map_err(|error| format!("could not write fixture stdout: {error}"))
}

fn write_stderr_lines(decoder: &mut Decoder) -> Result<(), String> {
    let count = decoder.u32()?;
    let mut stderr = std::io::stderr().lock();
    for _ in 0..count {
        stderr
            .write_all(&decoder.blob()?)
            .and_then(|()| stderr.write_all(b"\n"))
            .map_err(|error| format!("could not write fixture stderr: {error}"))?;
    }
    Ok(())
}

fn run() -> Result<i32, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not resolve fixture executable: {error}"))?;
    let config = std::fs::read(executable.with_extension("stub"))
        .map_err(|error| format!("could not read fixture config: {error}"))?;
    let mut decoder = Decoder::new(config);
    match decoder.byte()? {
        0 => Ok(0),
        1 => {
            write_stdout(&decoder.blob()?)?;
            Ok(0)
        }
        2 => {
            let exit_code = decoder.i32()?;
            write_stderr_lines(&mut decoder)?;
            Ok(exit_code)
        }
        3 => {
            let path = PathBuf::from(decoder.string()?);
            let stdout = decoder.blob()?;
            let mut args = std::env::args_os()
                .skip(1)
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("\n");
            if !args.is_empty() {
                args.push('\n');
            }
            std::fs::write(path, args)
                .map_err(|error| format!("could not record fixture argv: {error}"))?;
            write_stderr_lines(&mut decoder)?;
            write_stdout(&stdout)?;
            Ok(0)
        }
        4 => {
            let delay_ms = decoder.u64()?;
            let stdout = decoder.blob()?;
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            write_stdout(&stdout)?;
            Ok(0)
        }
        5 => {
            let path = PathBuf::from(decoder.string()?);
            let prefix = decoder.blob()?;
            let count = std::fs::read_to_string(&path)
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(0)
                .saturating_add(1);
            std::fs::write(&path, format!("{count}\n"))
                .map_err(|error| format!("could not update fixture counter: {error}"))?;
            write_stdout(&prefix)?;
            write_stdout(count.to_string().as_bytes())?;
            Ok(0)
        }
        tag => Err(format!("unknown fixture behavior tag {tag}")),
    }
}

fn main() {
    match run() {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(error) => {
            eprintln!("haider-stt native fixture failed: {error}");
            std::process::exit(125);
        }
    }
}
