use anyhow::Result;
use std::path::PathBuf;

const MARKER_BEGIN: &str = "# >>> shellanywhere >>>";
const MARKER_END: &str = "# <<< shellanywhere <<<";

/// Shell type detection and config injection
#[derive(Debug, Clone, Copy)]
pub enum ShellType {
    Bash,
    Zsh,
    Fish,
    Tcsh,
    Ksh,
    Nu,
    Elvish,
    Xonsh,
    PowerShell,
}

impl ShellType {
    /// Detect current shell type from environment
    pub fn detect() -> Option<Self> {
        #[cfg(unix)]
        {
            let shell = std::env::var("SHELL").ok()?;
            Self::from_path(&shell)
        }
        #[cfg(not(unix))]
        {
            // On Windows, try COMSPEC first; if it's an unsupported shell (e.g. cmd.exe),
            // fall back to PowerShell detection via PSModulePath
            if let Ok(comspec) = std::env::var("COMSPEC") {
                if let Some(st) = Self::from_path(&comspec) {
                    return Some(st);
                }
            }
            if std::env::var("PSModulePath").is_ok() {
                Some(Self::PowerShell)
            } else {
                None
            }
        }
    }

    /// Detect from a shell path
    pub fn from_path(path: &str) -> Option<Self> {
        let name = std::path::Path::new(path)
            .file_name()?
            .to_str()?
            .to_lowercase();

        match name.as_str() {
            "bash" | "bash.exe" => Some(Self::Bash),
            "zsh" | "zsh.exe" => Some(Self::Zsh),
            "fish" | "fish.exe" => Some(Self::Fish),
            "tcsh" | "csh" | "tcsh.exe" | "csh.exe" => Some(Self::Tcsh),
            "ksh" | "ksh93" | "mksh" | "ksh.exe" | "ksh93.exe" | "mksh.exe" => Some(Self::Ksh),
            "nu" | "nu.exe" => Some(Self::Nu),
            "elvish" | "elv" | "elvish.exe" => Some(Self::Elvish),
            "xonsh" | "xonsh.exe" => Some(Self::Xonsh),
            "powershell" | "pwsh" | "powershell.exe" | "pwsh.exe" => Some(Self::PowerShell),
            _ => None,
        }
    }

    /// Get the config file path for this shell
    pub fn config_path(&self) -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        match self {
            Self::Bash => Some(home.join(".bashrc")),
            Self::Zsh => Some(home.join(".zshrc")),
            Self::Fish => Some(home.join(".config").join("fish").join("config.fish")),
            Self::Tcsh => Some(home.join(".tcshrc")),
            Self::Ksh => Some(home.join(".kshrc")),
            Self::Nu => Some(home.join(".config").join("nushell").join("config.nu")),
            Self::Elvish => Some(home.join(".config").join("elvish").join("rc.elv")),
            Self::Xonsh => Some(home.join(".xonshrc")),
            Self::PowerShell => {
                let profile_dir = home.join("Documents").join("WindowsPowerShell");
                Some(profile_dir.join("Microsoft.PowerShell_profile.ps1"))
            }
        }
    }

    /// Generate the injection script
    pub fn injection_script(
        &self,
        exe_path: &str,
        server: Option<&str>,
        token: Option<&str>,
    ) -> String {
        let mut lines = Vec::new();
        lines.push(String::new()); // blank line before marker
        lines.push(MARKER_BEGIN.to_string());
        lines.push("# https://github.com/ejfkdev/ShellAnyWhere".to_string());

        match self {
            // Bash/Zsh: check SAW_SKIP, stdin is TTY, and interactive shell
            Self::Bash | Self::Zsh => {
                if let Some(s) = server {
                    lines.push(format!("export SAW_SERVER=\"{}\"", s));
                }
                if let Some(t) = token {
                    lines.push(format!("export SAW_TOKEN=\"{}\"", t));
                }
                lines.push(
                    "if [ -z \"$SAW_SKIP\" ] && [ -t 0 ] && [[ $- == *i* ]]; then".to_string(),
                );
                lines.push(format!("  exec {}", exe_path));
                lines.push("fi".to_string());
            }
            // Fish: check SAW_SKIP, stdin is TTY, and interactive
            Self::Fish => {
                if let Some(s) = server {
                    lines.push(format!("set -gx SAW_SERVER \"{}\"", s));
                }
                if let Some(t) = token {
                    lines.push(format!("set -gx SAW_TOKEN \"{}\"", t));
                }
                lines.push(
                    "if not set -q SAW_SKIP; and test -t 0; and status is-interactive".to_string(),
                );
                lines.push(format!("  exec {}", exe_path));
                lines.push("end".to_string());
            }
            // Tcsh: check SAW_SKIP, stdin is TTY, and interactive ($prompt is set)
            Self::Tcsh => {
                if let Some(s) = server {
                    lines.push(format!("setenv SAW_SERVER \"{}\"", s));
                }
                if let Some(t) = token {
                    lines.push(format!("setenv SAW_TOKEN \"{}\"", t));
                }
                lines.push("if (! $?SAW_SKIP && -t 0 && $?prompt) then".to_string());
                lines.push(format!("  exec {}", exe_path));
                lines.push("endif".to_string());
            }
            // Ksh: check SAW_SKIP, stdin is TTY, and interactive
            Self::Ksh => {
                if let Some(s) = server {
                    lines.push(format!("export SAW_SERVER=\"{}\"", s));
                }
                if let Some(t) = token {
                    lines.push(format!("export SAW_TOKEN=\"{}\"", t));
                }
                lines.push(
                    "if [ -z \"$SAW_SKIP\" ] && [ -t 0 ] && [[ -o interactive ]]; then".to_string(),
                );
                lines.push(format!("  exec {}", exe_path));
                lines.push("fi".to_string());
            }
            // Nu: check SAW_SKIP and interactive (Nu runs config.nu for all sessions,
            // but $nu.is-interactive distinguishes interactive from script mode)
            Self::Nu => {
                if let Some(s) = server {
                    lines.push(format!("$env.SAW_SERVER = \"{}\"", s));
                }
                if let Some(t) = token {
                    lines.push(format!("$env.SAW_TOKEN = \"{}\"", t));
                }
                lines.push(
                    "if (not ($env.SAW_SKIP? | is-not-empty)) and ($nu.is-interactive) {"
                        .to_string(),
                );
                lines.push(format!("  exec {}", exe_path));
                lines.push("}".to_string());
            }
            // Elvish: rc.elv is only sourced for interactive sessions
            Self::Elvish => {
                if let Some(s) = server {
                    lines.push(format!("set-env SAW_SERVER {}", s));
                }
                if let Some(t) = token {
                    lines.push(format!("set-env SAW_TOKEN {}", t));
                }
                lines.push("if (not has-env SAW_SKIP) {".to_string());
                lines.push(format!("  exec {}", exe_path));
                lines.push("}".to_string());
            }
            // Xonsh: check SAW_SKIP, stdin is TTY, and interactive
            Self::Xonsh => {
                if let Some(s) = server {
                    lines.push(format!("$SAW_SERVER = '{}'", s));
                }
                if let Some(t) = token {
                    lines.push(format!("$SAW_TOKEN = '{}'", t));
                }
                lines.push(
                    "if 'SAW_SKIP' not in ${^env} and sys.stdin.isatty() and $XONSH_INTERACTIVE:"
                        .to_string(),
                );
                lines.push(format!("    exec {}", exe_path));
            }
            // PowerShell: check SAW_SKIP, stdin not redirected, and ConsoleHost
            Self::PowerShell => {
                if let Some(s) = server {
                    lines.push(format!("$env:SAW_SERVER = \"{}\"", s));
                }
                if let Some(t) = token {
                    lines.push(format!("$env:SAW_TOKEN = \"{}\"", t));
                }
                lines.push("if (-not $env:SAW_SKIP -and -not [Console]::IsInputRedirected -and $Host.Name -eq 'ConsoleHost') {".to_string());
                lines.push(format!("  {}", exe_path));
                lines.push("}".to_string());
            }
        }

        lines.push(MARKER_END.to_string());
        lines.push(String::new()); // blank line after marker
        lines.join("\n")
    }
}

/// Inject shell-anywhere config into the detected shell's config file.
///
/// If saw-shell is already the default login shell ($SHELL), setup is
/// unnecessary — the system starts saw-shell directly. Prints a warning
/// and skips injection.
pub fn inject_config(
    shell: Option<String>,
    server: Option<String>,
    token: Option<String>,
) -> Result<()> {
    // Check if saw-shell is already the default shell (Unix only — $SHELL doesn't exist on Windows)
    #[cfg(unix)]
    if let Ok(default_shell) = std::env::var("SHELL") {
        if saw_core::util::guard::is_saw_executable(&default_shell) {
            println!(
                "saw-shell is already the default login shell ($SHELL={}).",
                default_shell
            );
            println!("No RC file injection needed — the system starts saw-shell directly.");
            println!("If you want to configure server/token, use the config file instead:");
            println!("  config dir/config.toml");
            return Ok(());
        }
    }

    let shell_type = shell
        .as_deref()
        .and_then(ShellType::from_path)
        .or_else(ShellType::detect)
        .ok_or_else(|| anyhow::anyhow!("Could not detect shell type. Use --shell to specify."))?;

    let config_path = shell_type.config_path().ok_or_else(|| {
        anyhow::anyhow!("Could not determine config file path for {:?}", shell_type)
    })?;

    let existing = if config_path.exists() {
        backup_file(&config_path)?;
        std::fs::read_to_string(&config_path)?
    } else {
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        String::new()
    };

    // Remove any previous injection (both old and new markers)
    let cleaned = remove_injection(&existing);

    let exe_path = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "saw-shell".to_string());

    let script = shell_type.injection_script(&exe_path, server.as_deref(), token.as_deref());
    let new_content = if cleaned.is_empty() {
        format!("{}\n", script)
    } else if cleaned.ends_with('\n') {
        format!("{}{}\n", script, cleaned)
    } else {
        format!("{}\n{}\n", script, cleaned)
    };

    std::fs::write(&config_path, new_content)?;

    println!("Injected ShellAnyWhere config into {:?}", config_path);
    println!("Shell type: {:?}", shell_type);
    println!("Open a new terminal to activate.");

    Ok(())
}

/// Remove ShellAnyWhere config from the detected shell's config file.
pub fn remove_config(shell: Option<String>) -> Result<()> {
    let shell_type = shell
        .as_deref()
        .and_then(ShellType::from_path)
        .or_else(ShellType::detect)
        .ok_or_else(|| anyhow::anyhow!("Could not detect shell type. Use --shell to specify."))?;

    let config_path = shell_type.config_path().ok_or_else(|| {
        anyhow::anyhow!("Could not determine config file path for {:?}", shell_type)
    })?;

    if !config_path.exists() {
        println!(
            "Config file {:?} does not exist, nothing to remove.",
            config_path
        );
        return Ok(());
    }

    let existing = std::fs::read_to_string(&config_path)?;
    let cleaned = remove_injection(&existing);

    if cleaned == existing {
        println!("No ShellAnyWhere config found in {:?}.", config_path);
        return Ok(());
    }

    backup_file(&config_path)?;

    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        std::fs::remove_file(&config_path)?;
        println!("Removed empty config file {:?}", config_path);
    } else {
        std::fs::write(&config_path, cleaned)?;
        println!("Removed ShellAnyWhere config from {:?}", config_path);
    }

    Ok(())
}

/// Remove the marked section from config content (handles both old and new markers).
/// Also removes blank lines immediately before/after the markers.
fn remove_injection(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut result = Vec::new();
    let mut skipping = false;

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed == MARKER_BEGIN {
            // Remove blank line before marker
            if !result.is_empty() && result.last().is_some_and(|l: &&str| l.trim().is_empty()) {
                result.pop();
            }
            skipping = true;
            i += 1;
            continue;
        }
        if trimmed == MARKER_END {
            skipping = false;
            // Remove blank line after marker
            if i + 1 < lines.len() && lines[i + 1].trim().is_empty() {
                i += 2; // skip marker end and the blank line after it
                continue;
            }
            i += 1;
            continue;
        }
        if !skipping {
            result.push(lines[i]);
        }
        i += 1;
    }

    let trimmed = result.join("\n").trim_end().to_string();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{}\n", trimmed)
    }
}

/// Backup a file by copying it to <path>.saw.bak (overwrites existing backup).
fn backup_file(path: &std::path::Path) -> Result<()> {
    let backup_path = with_extension_saw_bak(path);
    std::fs::copy(path, &backup_path)?;
    Ok(())
}

/// Compute the backup path: append .saw.bak to the filename.
/// e.g. .zshrc → .zshrc.saw.bak, config.fish → config.fish.saw.bak
fn with_extension_saw_bak(path: &std::path::Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    path.with_file_name(format!("{}.saw.bak", file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_type_from_path() {
        assert!(matches!(
            ShellType::from_path("/bin/zsh"),
            Some(ShellType::Zsh)
        ));
        assert!(matches!(
            ShellType::from_path("/bin/bash"),
            Some(ShellType::Bash)
        ));
        assert!(matches!(
            ShellType::from_path("/usr/local/bin/fish"),
            Some(ShellType::Fish)
        ));
        assert!(matches!(
            ShellType::from_path("/bin/tcsh"),
            Some(ShellType::Tcsh)
        ));
        assert!(matches!(
            ShellType::from_path("/bin/csh"),
            Some(ShellType::Tcsh)
        ));
        assert!(matches!(
            ShellType::from_path("/bin/ksh"),
            Some(ShellType::Ksh)
        ));
        assert!(matches!(
            ShellType::from_path("/bin/mksh"),
            Some(ShellType::Ksh)
        ));
        assert!(matches!(
            ShellType::from_path("/usr/bin/nu"),
            Some(ShellType::Nu)
        ));
        assert!(matches!(
            ShellType::from_path("/usr/bin/elvish"),
            Some(ShellType::Elvish)
        ));
        assert!(matches!(
            ShellType::from_path("/usr/bin/xonsh"),
            Some(ShellType::Xonsh)
        ));
        assert!(matches!(
            ShellType::from_path("powershell"),
            Some(ShellType::PowerShell)
        ));
        assert!(matches!(
            ShellType::from_path("pwsh"),
            Some(ShellType::PowerShell)
        ));
        assert!(ShellType::from_path("/bin/sh").is_none());
    }

    #[test]
    fn test_injection_script_zsh() {
        let script = ShellType::Zsh.injection_script(
            "/usr/local/bin/saw-shell",
            Some("my.server:18708"),
            Some("mytoken"),
        );
        assert!(script.contains(MARKER_BEGIN));
        assert!(script.contains(MARKER_END));
        assert!(script.contains("SAW_SERVER"));
        assert!(script.contains("SAW_TOKEN"));
        assert!(script.contains("SAW_SKIP"));
        assert!(script.contains("exec /usr/local/bin/saw-shell"));
        assert!(script.contains("[ -t 0 ]"));
        assert!(script.contains("$- == *i*"));
        assert!(!script.contains("exec saw-shell agent"));
    }

    #[test]
    fn test_injection_script_fish() {
        let script = ShellType::Fish.injection_script("saw-shell", None, None);
        assert!(script.contains("if not set -q SAW_SKIP"));
        assert!(script.contains("test -t 0"));
        assert!(script.contains("status is-interactive"));
        assert!(script.contains("end"));
        assert!(script.contains("exec saw-shell"));
    }

    #[test]
    fn test_injection_script_tcsh() {
        let script =
            ShellType::Tcsh.injection_script("saw-shell", Some("my.server:18708"), Some("mytoken"));
        assert!(script.contains("setenv SAW_SERVER"));
        assert!(script.contains("setenv SAW_TOKEN"));
        assert!(script.contains("SAW_SKIP"));
        assert!(script.contains("-t 0"));
        assert!(script.contains("$?prompt"));
        assert!(script.contains("endif"));
        assert!(script.contains("exec saw-shell"));
    }

    #[test]
    fn test_injection_script_nu() {
        let script = ShellType::Nu.injection_script("saw-shell", None, None);
        assert!(script.contains("$env.SAW_SKIP"));
        assert!(script.contains("$nu.is-interactive"));
        assert!(script.contains("exec saw-shell"));
    }

    #[test]
    fn test_injection_script_ksh() {
        let script =
            ShellType::Ksh.injection_script("saw-shell", Some("my.server:18708"), Some("mytoken"));
        assert!(script.contains("export SAW_SERVER"));
        assert!(script.contains("export SAW_TOKEN"));
        assert!(script.contains("SAW_SKIP"));
        assert!(script.contains("[ -t 0 ]"));
        assert!(script.contains("-o interactive"));
        assert!(script.contains("exec saw-shell"));
    }

    #[test]
    fn test_injection_script_elvish() {
        let script = ShellType::Elvish.injection_script("saw-shell", None, None);
        assert!(script.contains("has-env SAW_SKIP"));
        assert!(script.contains("exec saw-shell"));
    }

    #[test]
    fn test_injection_script_xonsh() {
        let script = ShellType::Xonsh.injection_script("saw-shell", None, None);
        assert!(script.contains("SAW_SKIP"));
        assert!(script.contains("sys.stdin.isatty()"));
        assert!(script.contains("$XONSH_INTERACTIVE"));
        assert!(script.contains("exec saw-shell"));
    }

    #[test]
    fn test_injection_script_powershell() {
        let script = ShellType::PowerShell.injection_script(
            "saw-shell.exe",
            Some("my.server:18708"),
            Some("mytoken"),
        );
        assert!(script.contains(MARKER_BEGIN));
        assert!(script.contains(MARKER_END));
        assert!(script.contains("$env:SAW_SERVER"));
        assert!(script.contains("$env:SAW_TOKEN"));
        assert!(script.contains("$env:SAW_SKIP"));
        assert!(script.contains("IsInputRedirected"));
        assert!(script.contains("ConsoleHost"));
        assert!(script.contains("saw-shell.exe"));
    }

    #[test]
    fn test_remove_injection() {
        let content = "# my config\n# >>> shellanywhere >>>\n# https://github.com/ejfkdev/ShellAnyWhere\nexport SAW_SERVER=test\nexec saw-shell\n# <<< shellanywhere <<<\nexport FOO=bar\n";
        let cleaned = remove_injection(content);
        assert!(!cleaned.contains("shellanywhere"));
        assert!(cleaned.contains("# my config"));
        assert!(cleaned.contains("export FOO=bar"));
    }

    #[test]
    fn test_remove_injection_no_marker() {
        let content = "export PATH=/usr/bin\nexport FOO=bar\n";
        let cleaned = remove_injection(content);
        assert_eq!(cleaned, content);
    }

    #[test]
    fn test_inject_and_remove_roundtrip() {
        let original = "export PATH=/usr/bin\n".to_string();
        let script = ShellType::Zsh.injection_script("saw-shell", None, None);
        let injected = format!("{}{}\n", script, original);

        let cleaned = remove_injection(&injected);
        assert_eq!(cleaned, original);
    }
}
