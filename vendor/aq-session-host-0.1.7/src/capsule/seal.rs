use super::*;
use sha2::{Digest as _, Sha256};
use std::io::{Read, Seek, Write};

pub(super) const SEALS: i32 =
    libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pin {
    pub digest: Digest,
    pub bytes: u64,
    pub device: u64,
    pub inode: u64,
    pub executable: bool,
}
pub(super) fn bytes(bytes: &[u8], executable: bool) -> Result<(File, Pin)> {
    ensure!(!bytes.is_empty(), "capsule empty image/input");
    let flags = libc::MFD_CLOEXEC
        | libc::MFD_ALLOW_SEALING
        | if executable {
            libc::MFD_EXEC
        } else {
            libc::MFD_NOEXEC_SEAL
        };
    let fd = unsafe { libc::memfd_create(c"aq-static-capsule".as_ptr(), flags) };
    ensure!(fd >= 0, "capsule memfd");
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.set_permissions(std::fs::Permissions::from_mode(if executable {
        0o500
    } else {
        0o400
    }))?;
    file.write_all(bytes)?;
    file.rewind()?;
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, SEALS) } == 0,
        "capsule seals"
    );
    let metadata = file.metadata()?;
    let pin = Pin {
        digest: Digest::of(bytes),
        bytes: bytes.len() as u64,
        device: metadata.dev(),
        inode: metadata.ino(),
        executable,
    };
    Ok((file, pin))
}
pub(super) fn source(
    path: &Path,
    expected: &ArtifactRef,
    bound: u64,
    static_elf: bool,
) -> Result<(File, Pin)> {
    expected.id.validate()?;
    expected.version.validate()?;
    ensure!(
        path.is_absolute() && std::fs::canonicalize(path)? == path,
        "capsule image canonical path"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file()
            && before.len() >= 64
            && before.len() <= bound
            && before.mode() & 0o111 != 0
            && before.mode() & 0o6022 == 0,
        "capsule image bounds/mode"
    );
    let mut body = Vec::new();
    Read::by_ref(&mut file)
        .take(bound + 1)
        .read_to_end(&mut body)?;
    ensure!(
        body.len() as u64 == before.len() && Digest::of(&body) == expected.digest,
        "capsule exact image bytes"
    );
    if static_elf {
        validate_static(&body)?;
    }
    bytes(&body, true)
}
pub(super) fn verify(file: &mut File, pin: &Pin, bound: u64) -> Result<()> {
    let metadata = file.metadata()?;
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    ensure!(
        metadata.is_file()
            && metadata.len() == pin.bytes
            && pin.bytes > 0
            && pin.bytes <= bound
            && metadata.dev() == pin.device
            && metadata.ino() == pin.inode
            && seals >= 0
            && seals & SEALS == SEALS,
        "capsule sealed descriptor mismatch"
    );
    ensure!(
        (metadata.mode() & 0o111 != 0) == pin.executable,
        "capsule descriptor exec mode"
    );
    file.rewind()?;
    let mut hash = Sha256::new();
    let mut scratch = [0u8; 65536];
    loop {
        let n = file.read(&mut scratch)?;
        if n == 0 {
            break;
        }
        hash.update(&scratch[..n]);
    }
    ensure!(
        format!("{:x}", hash.finalize()) == pin.digest.as_str(),
        "capsule sealed digest"
    );
    file.rewind()?;
    Ok(())
}
fn validate_static(body: &[u8]) -> Result<()> {
    ensure!(
        body.len() >= 64
            && &body[..6] == b"\x7fELF\x02\x01"
            && u16::from_le_bytes([body[18], body[19]]) == 62,
        "capsule requires x86_64 ELF"
    );
    let offset = usize::try_from(u64::from_le_bytes(body[32..40].try_into()?))?;
    let stride = u16::from_le_bytes(body[54..56].try_into()?) as usize;
    let count = u16::from_le_bytes(body[56..58].try_into()?) as usize;
    ensure!(
        stride >= 56 && count > 0 && count <= 128,
        "capsule ELF program header bounds"
    );
    for index in 0..count {
        let offset = offset
            .checked_add(index.checked_mul(stride).context("capsule ELF multiply")?)
            .context("capsule ELF offset")?;
        let end = offset.checked_add(4).context("capsule ELF header end")?;
        let kind = body
            .get(offset..end)
            .context("capsule truncated ELF header")?;
        ensure!(
            u32::from_le_bytes(kind.try_into()?) != 3,
            "capsule static profile rejects ELF interpreter"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_static_header_refuses_without_overflow_or_interpreter_fallback() {
        let mut body = vec![0u8; 128];
        body[..6].copy_from_slice(b"\x7fELF\x02\x01");
        body[18..20].copy_from_slice(&62u16.to_le_bytes());
        body[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        body[54..56].copy_from_slice(&56u16.to_le_bytes());
        body[56..58].copy_from_slice(&1u16.to_le_bytes());
        assert!(validate_static(&body).is_err());
        body[32..40].copy_from_slice(&64u64.to_le_bytes());
        body[64..68].copy_from_slice(&3u32.to_le_bytes());
        assert!(validate_static(&body).is_err());
    }
    #[test]
    fn sealed_exact_input_cannot_be_written_or_misidentified_as_program() {
        let (mut file, mut pin) = bytes(b"canary input", false).unwrap();
        verify(&mut file, &pin, 1024).unwrap();
        assert!(file.write_all(b"replacement").is_err());
        pin.executable = true;
        assert!(verify(&mut file, &pin, 1024).is_err());
    }
    #[test]
    fn ordinary_regular_file_is_not_mistaken_for_a_sealed_descriptor() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"unsealed canary").unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o400))
            .unwrap();
        let metadata = file.metadata().unwrap();
        let pin = Pin {
            digest: Digest::of(b"unsealed canary"),
            bytes: metadata.len(),
            device: metadata.dev(),
            inode: metadata.ino(),
            executable: false,
        };
        assert!(verify(&mut file, &pin, 1024).is_err());
    }
}
