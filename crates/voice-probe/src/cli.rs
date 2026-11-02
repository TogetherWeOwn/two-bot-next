//! Argument parsing for the `voice-probe` binary.
//!
//! Pure by design: `parse` takes the argument words and an environment
//! lookup, so every flag/env combination is a unit test and the binary is a
//! thin async wrapper. Precedence is flag, then environment, then default.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};

use crate::runner::DEFAULT_SSRC;

/// Usage text printed for `--help` and on usage errors.
pub const USAGE: &str = "\
Usage: voice-probe [--mode probe|echo] [options]\n\
\n\
Probe (default): exchange Discord-shaped UDP with a controlled echo and\n\
print one JSON evidence object on stdout. Exit 0 on a completed run;\n\
exit 1 only on fatal errors (unresolvable echo, socket failure).\n\
  --echo <host:port>   echo target (or ECHO_HOST)\n\
  --duration-s <n>     RTT send window in seconds [300] (or DURATION_S)\n\
  --rate-s <n>         RTT packets per second [10] (or RATE_S, 1-1000)\n\
  --ssrc <n>           discovery SSRC [default]\n\
\n\
Echo: serve discovery replies plus verbatim echo until stopped.\n\
Short-lived probe runs only; never public in production.\n\
  --mode echo --bind <addr:port>  bind address [0.0.0.0:9001]\n\
\n\
  --help               print this text and exit 0\n\
";

/// Default echo bind: all interfaces, probe port 9001.
pub const DEFAULT_ECHO_BIND: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9001);

/// Highest accepted send rate: 100x the default, 20x RTP. Past this the
/// 1 s tick truncates toward zero (past 1e9 it panics the interval), and
/// the receipt ledger outgrows a short-lived probe run.
pub const MAX_RATE_S: u32 = 1000;

/// Parsed probe invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeArgs {
    /// Echo target (`host:port`).
    pub echo: String,
    /// RTT send window in seconds.
    pub duration_s: u64,
    /// RTT packets per second.
    pub rate_s: u32,
    /// Discovery SSRC.
    pub ssrc: u32,
}

/// Parsed echo invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoArgs {
    /// Address to bind.
    pub bind: SocketAddr,
}

/// One parsed invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print [`USAGE`] and exit 0.
    Help,
    /// Run the probe.
    Probe(ProbeArgs),
    /// Serve the echo.
    Echo(EchoArgs),
}

/// Parse `args` (without the program name). `env` reads the environment;
/// the binary passes the process environment, tests pass a stub.
pub fn parse(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<Command, String> {
    let mut flags: HashMap<String, String> = HashMap::new();
    let mut words = args.iter();
    while let Some(word) = words.next() {
        if word == "--help" || word == "-h" {
            return Ok(Command::Help);
        }
        let Some(name) = word.strip_prefix("--") else {
            return Err(format!("unexpected argument: {word}"));
        };
        let (name, inline) = match name.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (name, None),
        };
        if !matches!(
            name,
            "mode" | "echo" | "bind" | "duration-s" | "rate-s" | "ssrc"
        ) {
            return Err(format!("unknown argument: --{name}"));
        }
        let value = match inline {
            Some(value) => value,
            None => match words.next() {
                Some(next) if !next.starts_with('-') => next.clone(),
                _ => return Err(format!("missing value for --{name}")),
            },
        };
        flags.insert(name.to_owned(), value);
    }

    match flags.get("mode").map(String::as_str).unwrap_or("probe") {
        "probe" => parse_probe(&flags, env),
        "echo" => parse_echo(&flags),
        mode => Err(format!("unknown --mode: {mode} (want probe or echo)")),
    }
}

/// Build probe arguments; flags win over the environment.
fn parse_probe(
    flags: &HashMap<String, String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Command, String> {
    if flags.contains_key("bind") {
        return Err("--bind is echo-only".to_owned());
    }
    let echo = flags
        .get("echo")
        .cloned()
        .or_else(|| env("ECHO_HOST"))
        .ok_or_else(|| "missing --echo <host:port> (or ECHO_HOST)".to_owned())?;
    if !echo.contains(':') {
        return Err(format!("--echo must be host:port, got {echo}"));
    }
    let duration_s: u64 = number(
        flags.get("duration-s"),
        env("DURATION_S"),
        "--duration-s",
        300,
    )?;
    if duration_s < 1 {
        return Err("--duration-s must be at least 1".to_owned());
    }
    let rate_s: u32 = number(flags.get("rate-s"), env("RATE_S"), "--rate-s", 10)?;
    if !(1..=MAX_RATE_S).contains(&rate_s) {
        return Err(format!("--rate-s must be between 1 and {MAX_RATE_S}"));
    }
    let ssrc: u32 = number(flags.get("ssrc"), None, "--ssrc", DEFAULT_SSRC)?;
    Ok(Command::Probe(ProbeArgs {
        echo,
        duration_s,
        rate_s,
        ssrc,
    }))
}

/// Build echo arguments; the echo takes no probe flags.
fn parse_echo(flags: &HashMap<String, String>) -> Result<Command, String> {
    for name in ["echo", "duration-s", "rate-s", "ssrc"] {
        if flags.contains_key(name) {
            return Err(format!("--{name} is probe-only"));
        }
    }
    let bind = match flags.get("bind") {
        Some(text) => text
            .parse::<SocketAddr>()
            .map_err(|_| format!("--bind must be an address:port, got {text}"))?,
        None => DEFAULT_ECHO_BIND,
    };
    Ok(Command::Echo(EchoArgs { bind }))
}

/// Resolve one numeric option: flag, then environment, then default.
fn number<T>(
    flag: Option<&String>,
    env: Option<String>,
    name: &str,
    default: T,
) -> Result<T, String>
where
    T: std::str::FromStr + Copy,
{
    match flag.cloned().or(env) {
        Some(text) => text
            .parse::<T>()
            .map_err(|_| format!("{name} must be a number, got {text}")),
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::{Ipv4Addr, SocketAddr};

    use super::*;
    use crate::runner::DEFAULT_SSRC;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(ToString::to_string).collect()
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn empty_env() -> impl Fn(&str) -> Option<String> {
        env(&[])
    }

    #[test]
    fn help_short_circuits() {
        for words in [&["--help"][..], &["-h"], &["--help", "--echo", "x:1"]] {
            assert!(matches!(
                parse(&args(words), &empty_env()),
                Ok(Command::Help)
            ));
        }
    }

    #[test]
    fn probe_defaults() {
        let command = parse(&args(&["--echo", "echo.example:9001"]), &empty_env()).unwrap();

        assert!(matches!(
            command,
            Command::Probe(ProbeArgs {
                echo,
                duration_s: 300,
                rate_s: 10,
                ssrc,
            }) if echo == "echo.example:9001" && ssrc == DEFAULT_SSRC
        ));
    }

    #[test]
    fn probe_flags_override_defaults() {
        let command = parse(
            &args(&[
                "--echo",
                "echo.example:9001",
                "--duration-s",
                "60",
                "--rate-s",
                "20",
                "--ssrc",
                "99",
            ]),
            &empty_env(),
        )
        .unwrap();

        assert!(matches!(
            command,
            Command::Probe(ProbeArgs {
                duration_s: 60,
                rate_s: 20,
                ssrc: 99,
                ..
            })
        ));
    }

    #[test]
    fn probe_accepts_equals_form() {
        let command = parse(
            &args(&["--echo=echo.example:9001", "--duration-s=60"]),
            &empty_env(),
        )
        .unwrap();

        assert!(matches!(
            command,
            Command::Probe(ProbeArgs { duration_s: 60, .. })
        ));
    }

    #[test]
    fn probe_env_fills_gaps() {
        let environment = env(&[
            ("ECHO_HOST", "env.example:9001"),
            ("DURATION_S", "45"),
            ("RATE_S", "5"),
        ]);

        let command = parse(&args(&[]), &environment).unwrap();

        assert!(matches!(
            command,
            Command::Probe(ProbeArgs {
                echo,
                duration_s: 45,
                rate_s: 5,
                ..
            }) if echo == "env.example:9001"
        ));
    }

    #[test]
    fn probe_flag_beats_env() {
        let environment = env(&[("ECHO_HOST", "env.example:9001"), ("RATE_S", "5")]);

        let command = parse(
            &args(&["--echo", "flag.example:9001", "--rate-s", "7"]),
            &environment,
        )
        .unwrap();

        assert!(matches!(
            command,
            Command::Probe(ProbeArgs {
                echo,
                rate_s: 7,
                ..
            }) if echo == "flag.example:9001"
        ));
    }

    #[test]
    fn probe_requires_echo() {
        let error = parse(&args(&[]), &empty_env()).unwrap_err();

        assert!(error.contains("--echo"), "was: {error}");
        assert!(error.contains("ECHO_HOST"), "was: {error}");
    }

    #[test]
    fn probe_rejects_echo_without_port() {
        let error = parse(&args(&["--echo", "echo.example"]), &empty_env()).unwrap_err();

        assert!(error.contains("host:port"), "was: {error}");
    }

    #[test]
    fn probe_rejects_bad_numbers() {
        let base = ["--echo", "echo.example:9001"];
        for flag in ["--duration-s", "--rate-s", "--ssrc"] {
            let mut words = base.to_vec();
            words.extend([flag, "abc"]);
            assert!(parse(&args(&words), &empty_env()).is_err(), "{flag} abc");
        }
        let mut words = base.to_vec();
        words.extend(["--duration-s", "0"]);
        let error = parse(&args(&words), &empty_env()).unwrap_err();
        assert!(error.contains("at least 1"), "was: {error}");
        let mut words = base.to_vec();
        words.extend(["--ssrc", "99999999999"]);
        assert!(parse(&args(&words), &empty_env()).is_err());
    }

    #[test]
    fn probe_bounds_rate() {
        let base = ["--echo", "echo.example:9001"];
        // Absurd rates truncate the send tick to zero and panic; the parser
        // refuses them instead of letting the runner die at runtime.
        for bad in ["0", "1001", "2000000000"] {
            let mut words = base.to_vec();
            words.extend(["--rate-s", bad]);
            let error = parse(&args(&words), &empty_env()).unwrap_err();
            assert!(
                error.contains("between 1 and 1000"),
                "--rate-s {bad} was: {error}"
            );
        }
        let mut words = base.to_vec();
        words.extend(["--rate-s", "1000"]);
        assert!(matches!(
            parse(&args(&words), &empty_env()),
            Ok(Command::Probe(ProbeArgs { rate_s: 1000, .. }))
        ));
    }

    #[test]
    fn rejects_unknown_mode_flag_and_positional() {
        assert!(parse(&args(&["--mode", "frobnicate"]), &empty_env()).is_err());
        assert!(parse(&args(&["--bogus", "1"]), &empty_env()).is_err());
        assert!(parse(&args(&["-x"]), &empty_env()).is_err());
        assert!(parse(&args(&["positional"]), &empty_env()).is_err());
        assert!(parse(&args(&["--echo"]), &empty_env()).is_err());
        assert!(parse(&args(&["--echo", "--mode", "probe"]), &empty_env()).is_err());
    }

    #[test]
    fn echo_defaults_to_all_interfaces_probe_port() {
        let command = parse(&args(&["--mode", "echo"]), &empty_env()).unwrap();

        assert!(matches!(
            command,
            Command::Echo(EchoArgs { bind }) if bind == SocketAddr::from((Ipv4Addr::UNSPECIFIED, 9001))
        ));
    }

    #[test]
    fn echo_accepts_custom_bind_and_rejects_bad_bind() {
        let command = parse(
            &args(&["--mode", "echo", "--bind", "127.0.0.1:0"]),
            &empty_env(),
        )
        .unwrap();
        assert!(matches!(
            command,
            Command::Echo(EchoArgs { bind }) if bind == SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
        ));

        assert!(parse(&args(&["--mode", "echo", "--bind", "nope"]), &empty_env()).is_err());
    }

    #[test]
    fn rejects_mode_mismatched_flags() {
        assert!(parse(
            &args(&["--echo", "e:1", "--bind", "127.0.0.1:1"]),
            &empty_env()
        )
        .is_err());
        for flag in ["--echo", "--duration-s", "--rate-s", "--ssrc"] {
            assert!(
                parse(&args(&["--mode", "echo", flag, "1"]), &empty_env()).is_err(),
                "{flag} in echo mode"
            );
        }
    }

    #[test]
    fn usage_names_every_flag_and_env() {
        for token in [
            "--mode",
            "--echo",
            "--bind",
            "--duration-s",
            "--rate-s",
            "--ssrc",
            "--help",
            "ECHO_HOST",
            "DURATION_S",
            "RATE_S",
        ] {
            assert!(USAGE.contains(token), "usage lacks {token}");
        }
    }
}
