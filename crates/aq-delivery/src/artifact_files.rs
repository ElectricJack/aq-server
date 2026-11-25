//! Durable staging and ranged reads shared by worker source and lifecycle uploads.
use sha2::{Digest as _, Sha256};
use std::{io, path::Path};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub async fn append(path: &Path, offset: u64, bytes: &[u8]) -> io::Result<()> {
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "artifact path has no directory",
        )
    })?;
    tokio::fs::create_dir_all(directory).await?;
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .await?;
    // Discard any unacknowledged bytes from a crash before the SQL commit.
    f.set_len(offset).await?;
    f.seek(std::io::SeekFrom::Start(offset)).await?;
    f.write_all(bytes).await?;
    f.sync_all().await?;
    Ok(())
}

pub async fn digest(path: &Path) -> io::Result<(String, u64)> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex::encode(h.finalize()), total))
}

/// Read at most the requested bytes. A lifecycle fetch may observe EOF before the requested end.
pub async fn read(path: &Path, offset: u64, want: usize) -> io::Result<Vec<u8>> {
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        let n = f.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}
