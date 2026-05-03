use lexopt::prelude::*;

/// Parsed CLI arguments
pub enum Cli {
    Agent(AgentArgs),
    Install(InstallArgs),
    Uninstall(UninstallArgs),
    Help,
    Version,
}

pub struct AgentArgs {
    pub server: Option<String>,
    pub token: Option<String>,
    pub shell: Option<String>,
    pub no_ssh_key_forward: bool,
    pub flush_interval: u64,
    pub io_compress: bool,
    pub io_diff: bool,
}

pub struct InstallArgs {
    pub server: Option<String>,
    pub token: Option<String>,
    pub shell: Option<String>,
}

pub struct UninstallArgs {
    pub shell: Option<String>,
}

const HELP_TEXT: &str = "\
saw-shell — ShellAnyWhere remote shell agent

https://github.com/ejfkdev/ShellAnyWhere

USAGE:
    saw-shell [OPTIONS]
    saw-shell install [OPTIONS]
    saw-shell uninstall [OPTIONS]
    saw-shell -h | --help
    saw-shell -V | --version

OPTIONS:
    -s, --server <ADDR>            Server address (env: SAW_SERVER)
    -t, --token <TOKEN>            Auth token (env: SAW_TOKEN)
        --shell <PATH>             Shell program path (env: SAW_SHELL_PATH)
        --flush-interval <N>       Output flush interval in ms [default: 100]
        --no-ssh-key-forward       Disable SSH public key forwarding (env: SAW_NO_SSH_KEY_FORWARD)
        --io-compress              Enable TerminalIO output compression (lz4) (env: SAW_IO_COMPRESS)
        --io-diff                  Enable diff optimization for fullscreen programs (env: SAW_IO_DIFF)

INSTALL:
    saw-shell install           Write connection config into shell RC files
    saw-shell install -s ADDR   Write with specific server address
    saw-shell install -t TOKEN  Write with specific token

UNINSTALL:
    saw-shell uninstall         Remove ShellAnyWhere config from shell RC files

EXAMPLES:
    saw-shell                                       Connect with defaults
    saw-shell -s my.server:18708 -t abc123          Connect to specific server
    saw-shell --io-compress                         Enable output compression
    saw-shell install -s my.server:18708 -t abc     Write config to shell RC files
    saw-shell uninstall                             Remove config from shell RC files";

pub fn parse() -> Result<Cli, lexopt::Error> {
    let mut args = lexopt::Parser::from_env();

    match args.next()? {
        Some(Value(val)) => {
            let s = val.string()?;
            if s == "install" {
                parse_install_inner(args)
            } else if s == "uninstall" {
                parse_uninstall_inner(args)
            } else if s == "setup" {
                // Backward compat alias
                parse_install_inner(args)
            } else if s == "help" {
                Ok(Cli::Help)
            } else {
                Err(lexopt::Error::UnexpectedArgument(
                    format!("unknown subcommand: {s}\nTry 'saw-shell --help' for usage.").into(),
                ))
            }
        }
        Some(Long("help")) => Ok(Cli::Help),
        Some(Short('h')) => Ok(Cli::Help),
        Some(Long("version")) => Ok(Cli::Version),
        Some(Short('V')) => Ok(Cli::Version),
        Some(Long(_) | Short(_)) => parse_agent_from_scratch(),
        None => Ok(Cli::Agent(AgentArgs {
            server: None,
            token: None,
            shell: None,
            no_ssh_key_forward: false,
            flush_interval: 0,
            io_compress: false,
            io_diff: false,
        })),
    }
}

fn parse_agent_from_scratch() -> Result<Cli, lexopt::Error> {
    let mut args = lexopt::Parser::from_env();
    let mut server = None;
    let mut token = None;
    let mut shell = None;
    let mut no_ssh_key_forward = false;
    let mut flush_interval: u64 = 0;
    let mut io_compress = false;
    let mut io_diff = false;

    while let Some(arg) = args.next()? {
        match arg {
            Long("help") | Short('h') => return Ok(Cli::Help),
            Long("version") | Short('V') => return Ok(Cli::Version),
            Long(name) => match name {
                "server" => server = Some(args.value()?.string()?),
                "token" => token = Some(args.value()?.string()?),
                "shell" => shell = Some(args.value()?.string()?),
                "no-ssh-key-forward" => no_ssh_key_forward = true,
                "flush-interval" => flush_interval = args.value()?.parse()?,
                "io-compress" => io_compress = true,
                "io-diff" => io_diff = true,
                _ => return Err(lexopt::Error::UnexpectedArgument(name.into())),
            },
            Short(s) => match s {
                's' => server = Some(args.value()?.string()?),
                't' => token = Some(args.value()?.string()?),
                _ => return Err(lexopt::Error::UnexpectedArgument(s.to_string().into())),
            },
            Value(v) => return Err(lexopt::Error::UnexpectedArgument(v)),
        }
    }

    Ok(Cli::Agent(AgentArgs {
        server,
        token,
        shell,
        no_ssh_key_forward,
        flush_interval,
        io_compress,
        io_diff,
    }))
}

fn parse_install_inner(mut args: lexopt::Parser) -> Result<Cli, lexopt::Error> {
    let mut server = None;
    let mut token = None;
    let mut shell = None;

    while let Some(arg) = args.next()? {
        match arg {
            Long("help") | Short('h') => return Ok(Cli::Help),
            Long(name) => match name {
                "server" => server = Some(args.value()?.string()?),
                "token" => token = Some(args.value()?.string()?),
                "shell" => shell = Some(args.value()?.string()?),
                _ => return Err(lexopt::Error::UnexpectedArgument(name.into())),
            },
            Short(s) => match s {
                's' => server = Some(args.value()?.string()?),
                't' => token = Some(args.value()?.string()?),
                _ => return Err(lexopt::Error::UnexpectedArgument(s.to_string().into())),
            },
            Value(v) => return Err(lexopt::Error::UnexpectedArgument(v)),
        }
    }

    Ok(Cli::Install(InstallArgs {
        server,
        token,
        shell,
    }))
}

fn parse_uninstall_inner(mut args: lexopt::Parser) -> Result<Cli, lexopt::Error> {
    let mut shell = None;

    while let Some(arg) = args.next()? {
        match arg {
            Long("help") | Short('h') => return Ok(Cli::Help),
            Long(name) => match name {
                "shell" => shell = Some(args.value()?.string()?),
                _ => return Err(lexopt::Error::UnexpectedArgument(name.into())),
            },
            Value(v) => return Err(lexopt::Error::UnexpectedArgument(v)),
            Short(s) => return Err(lexopt::Error::UnexpectedArgument(s.to_string().into())),
        }
    }

    Ok(Cli::Uninstall(UninstallArgs { shell }))
}

pub fn print_help() {
    print!("{HELP_TEXT}");
}

pub fn print_version() {
    println!("saw-shell {}", env!("CARGO_PKG_VERSION"));
}
