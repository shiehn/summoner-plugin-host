//! The architecture check, from the bundle executable's own Mach-O header, BEFORE anything of
//! the bundle is loaded (no `CFBundleCreate`, no `dlopen`): an x86_64-only bundle is refused with zero loads, never
//! after Rosetta has been asked. It reads the first bytes of one file inside a bundle the lab gate already passed.
//! It never walks `~/` or the system plugin folders.
use std::io::Read;
use std::path::{Path, PathBuf};

const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const CPU_TYPE_X86_64: u32 = 0x0100_0007;

/// What a Mach-O file can run as here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arch {
    /// It has an arm64 slice (thin, or one slice of a fat file).
    Arm64,
    /// No arm64 slice; the CPU types it has.
    Other(Vec<u32>),
    /// Not a Mach-O file (or too short to tell).
    NotMachO,
}

/// The architectures of the Mach-O file `exe`, from its header alone.
pub fn arch_of(exe: &Path) -> std::io::Result<Arch> {
    let mut head = Vec::with_capacity(4096);
    std::fs::File::open(exe)?
        .take(4096)
        .read_to_end(&mut head)?;
    Ok(parse(&head))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|x| u32::from_be_bytes([x[0], x[1], x[2], x[3]]))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}

/// The header, parsed: thin 64-bit (little-endian `MH_MAGIC_64`), fat (`FAT_MAGIC`) or fat64 (`FAT_MAGIC_64`).
pub fn parse(b: &[u8]) -> Arch {
    let types: Vec<u32> = match be32(b, 0) {
        Some(0xcafe_babe) | Some(0xcafe_babf) => {
            let wide = be32(b, 0) == Some(0xcafe_babf);
            let n = be32(b, 4).unwrap_or(0).min(64) as usize;
            let stride = if wide { 32 } else { 20 };
            (0..n).filter_map(|i| be32(b, 8 + i * stride)).collect()
        }
        _ => match le32(b, 0) {
            Some(0xfeed_facf) | Some(0xfeed_face) => le32(b, 4).into_iter().collect(),
            _ => return Arch::NotMachO,
        },
    };
    if types.is_empty() {
        Arch::NotMachO
    } else if types.contains(&CPU_TYPE_ARM64) {
        Arch::Arm64
    } else {
        Arch::Other(types)
    }
}

/// The bundle's executable: the one regular file in `Contents/MacOS` (the lab's bundles have exactly one; a bundle
/// with none or several is refused as ambiguous rather than guessed).
pub fn executable(bundle: &Path) -> Result<PathBuf, crate::Error> {
    executable_why(bundle).map_err(crate::Error::from)
}

fn executable_why(bundle: &Path) -> Result<PathBuf, String> {
    let dir = bundle.join("Contents/MacOS");
    let files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .collect();
    match files.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(format!(
            "{}: {} executables, not one",
            dir.display(),
            files.len()
        )),
    }
}

/// `x86_64` for the report.
pub fn cpu_name(t: u32) -> String {
    match t {
        CPU_TYPE_ARM64 => "arm64".into(),
        CPU_TYPE_X86_64 => "x86_64".into(),
        t => format!("cpu {t:#x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_parse() {
        let thin_arm = [0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01];
        assert_eq!(parse(&thin_arm), Arch::Arm64);
        let thin_x86 = [0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01];
        assert_eq!(parse(&thin_x86), Arch::Other(vec![CPU_TYPE_X86_64]));
        let mut fat = vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2];
        fat.extend_from_slice(&[0x01, 0, 0, 0x07]);
        fat.extend_from_slice(&[0; 16]);
        fat.extend_from_slice(&[0x01, 0, 0, 0x0c]);
        fat.extend_from_slice(&[0; 16]);
        assert_eq!(parse(&fat), Arch::Arm64, "a universal binary");
        assert_eq!(parse(b"#!/bin/sh\n"), Arch::NotMachO);
        assert_eq!(parse(&[]), Arch::NotMachO);
    }

    #[test]
    fn this_test_binary_is_arm64() {
        assert_eq!(
            arch_of(&std::env::current_exe().unwrap()).unwrap(),
            Arch::Arm64
        );
    }
}
