//! V1 Codex TUI quirks; callers provide the actual current viewport, never history.
pub const READY_PROMPT_PREFIX: &str = "› ";
pub const READY_DELAY_MS: u64 = 3000;
pub const COMPOSER_CLEAR: &[u8] = &[0x15]; // C-u; Escape clears/backtracks Codex input.
pub const SKIP_ESCAPE_BEFORE_ENTER: bool = true;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteractiveViewport {
    Ready,
    TrustDirectory,
    AuthenticationRequired,
    Waiting,
}
pub fn classify_current_viewport(view: &str) -> InteractiveViewport {
    let lower = view.to_lowercase();
    if lower.contains("sign in with chatgpt")
        || lower.contains("codex login")
        || lower.contains("log out and sign in again")
    {
        InteractiveViewport::AuthenticationRequired
    } else if view.contains("Do you trust the contents of this directory")
        || view.contains("Trust this folder?")
    {
        InteractiveViewport::TrustDirectory
    } else if view
        .lines()
        .any(|line| line.trim_start().starts_with(READY_PROMPT_PREFIX))
    {
        InteractiveViewport::Ready
    } else {
        InteractiveViewport::Waiting
    }
}
