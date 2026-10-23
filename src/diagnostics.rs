use tokio::io::{AsyncRead, AsyncReadExt};

pub async fn prefix(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut reader).take(8192).read_to_end(&mut bytes).await?;
    tokio::io::copy(&mut reader, &mut tokio::io::sink()).await?;
    Ok(bytes)
}

pub fn import_category(bytes: &[u8]) -> &'static str {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    if text.contains("bwrap:") {
        return "IMPORT_SANDBOX_REJECTED";
    }
    if text.contains("not enough space")
        || text.contains("no space left")
        || text.contains("could not allocate")
    {
        return "VOLUME_CAPACITY";
    }
    if text.contains("permission denied") {
        return "SOURCE_PERMISSION";
    }
    if text.contains("no such file or directory") {
        return "SOURCE_DISAPPEARED";
    }
    "DIRECTORY_IMPORT_FAILED"
}

pub fn backend_category(bytes: &[u8]) -> &'static str {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    if text.contains("unexpected argument") || text.contains("unrecognized option") {
        return "VIRTIOFSD_VERSION_UNSUPPORTED";
    }
    if text.contains("bwrap:") || text.contains("sandbox") || text.contains("namespace") {
        return "BACKEND_SANDBOX_REJECTED";
    }
    if text.contains("permission denied") {
        return "BACKEND_PERMISSION";
    }
    if text.contains("seccomp") {
        return "BACKEND_SECCOMP";
    }
    "BACKEND_EXITED"
}
