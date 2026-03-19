use anyhow::{Context, Result};
use diffy::{apply, Patch};

pub fn create_patch(previous: &str, current: &str) -> Vec<u8> {
    diffy::create_patch(previous, current)
        .to_string()
        .into_bytes()
}

pub fn apply_patch(previous: &str, patch_bytes: &[u8]) -> Result<String> {
    let patch_text =
        std::str::from_utf8(patch_bytes).context("patch payload is not valid UTF-8")?;
    let patch = Patch::from_str(patch_text).context("failed to parse stored patch")?;
    apply(previous, &patch).context("failed to apply stored patch")
}
