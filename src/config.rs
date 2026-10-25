use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

fn token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("discord-gtk").join("token"))
}

pub fn load_token() -> Option<String> {
    if let Ok(t) = std::env::var("DISCORD_TOKEN") {
        if !t.trim().is_empty() {
            return Some(t.trim().to_owned());
        }
    }
    let t = fs::read_to_string(token_path()?).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

pub fn save_token(token: &str) -> std::io::Result<()> {
    let path = token_path().ok_or_else(|| std::io::Error::other("no config directory"))?;
    fs::create_dir_all(path.parent().expect("token path has a parent"))?;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(token.as_bytes())
}

pub fn forget_token() {
    if let Some(p) = token_path() {
        let _ = fs::remove_file(p);
    }
}
