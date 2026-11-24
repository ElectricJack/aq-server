use super::*;

pub(super) fn auth(source: &CodexSource, now: u64) -> Option<Zeroizing<String>> {
    let bytes = read_auth_file(&source.auth_file)?;
    let mut v: Value = serde_json::from_slice(&bytes).ok()?;
    let tokens = v.get_mut("tokens")?;
    if tokens.get("account_id")?.as_str()? != source.chatgpt_account_id {
        return None;
    }
    let Value::String(token) = tokens.get_mut("access_token")?.take() else {
        return None;
    };
    let token = Zeroizing::new(token);
    if !valid_text(&token, 16_384) || !token.is_ascii() {
        return None;
    }
    check_expiry(&token, now)?;
    Some(token)
}
fn read_auth_file(path: &std::path::Path) -> Option<Zeroizing<Vec<u8>>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    let private = metadata.mode() & 0o077 == 0 && metadata.uid() == nix::unistd::geteuid().as_raw();
    if !metadata.is_file() || !private {
        return None;
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return None;
    }
    Some(bytes)
}
fn check_expiry(token: &str, now: u64) -> Option<()> {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() == 3 {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .ok()?;
        let claims: Value = serde_json::from_slice(&payload).ok()?;
        if claims
            .get("exp")
            .and_then(Value::as_u64)
            .is_none_or(|exp| exp <= now / 1000)
        {
            return None;
        }
    }
    Some(())
}
