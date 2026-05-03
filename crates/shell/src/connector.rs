/// Read SSH public keys from ~/.ssh/ and config directory.
/// Returns a list of public key lines (e.g. "ssh-ed25519 AAAA... user@host").
pub fn read_ssh_public_keys(config_dir: &std::path::Path) -> Vec<String> {
    let mut keys = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Helper to add a key line with dedup
    let mut add_key = |line: &str| {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') && seen.insert(trimmed.to_string()) {
            keys.push(trimmed.to_string());
        }
    };

    // Read from ~/.ssh/*.pub files
    if let Some(home) = dirs::home_dir() {
        let ssh_dir = home.join(".ssh");
        if ssh_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&ssh_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "pub")
                        && let Ok(content) = std::fs::read_to_string(&path)
                    {
                        for line in content.lines() {
                            add_key(line);
                        }
                    }
                }
            }
            // Also read authorized_keys
            let authorized_keys = ssh_dir.join("authorized_keys");
            if let Ok(content) = std::fs::read_to_string(&authorized_keys) {
                for line in content.lines() {
                    add_key(line);
                }
            }
        }
    }

    // Read from config directory authorized_keys
    let config_authorized_keys = config_dir.join("authorized_keys");
    if let Ok(content) = std::fs::read_to_string(&config_authorized_keys) {
        for line in content.lines() {
            add_key(line);
        }
    }

    keys
}
