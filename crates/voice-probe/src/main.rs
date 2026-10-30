//! `voice-probe`: probe mode prints one JSON evidence object to stdout;
//! echo mode serves discovery replies plus verbatim echo until stopped.
//!
//! Exit codes: 0 completes a run (loss and discovery failure are evidence,
//! not failures), 1 marks fatal errors (unresolvable echo, socket or
//! stdout failure), 2 marks usage errors. Stdout carries only the result
//! text, written through an explicit handle (the workspace denies the
//! print macros); diagnostics and usage use stderr.

use std::time::Duration;

use two_bot_voice_probe::{cli, evidence, runner};

/// Resolve one invocation to its stdout text.
async fn execute(command: cli::Command) -> Result<String, String> {
    match command {
        cli::Command::Help => Ok(cli::USAGE.to_owned()),
        cli::Command::Probe(args) => {
            let config = runner::ProbeConfig {
                duration: Duration::from_secs(args.duration_s),
                rate_per_sec: args.rate_s,
                ssrc: args.ssrc,
                ..runner::ProbeConfig::new(args.echo)
            };
            runner::run_probe(&config)
                .await
                .map(|result| evidence::render(&result) + "\n")
                .map_err(|error| error.to_string())
        }
        cli::Command::Echo(args) => {
            let config = runner::EchoConfig { bind: args.bind };
            match runner::run_echo(&config, wait_for_shutdown()).await {
                Ok(stats) => {
                    eprintln!(
                        "voice-probe: echo served {} discovery replies, {} echoes",
                        stats.discovery_replies, stats.echoed
                    );
                    Ok(String::new())
                }
                Err(error) => Err(format!("echo failed: {error}")),
            }
        }
    }
}

/// Container stop (SIGTERM) or Ctrl-C (SIGINT).
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = &mut ctrl_c => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Write result text to stdout; a short write is an error like any other.
fn write_stdout(text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}

#[tokio::main]
async fn main() {
    let words: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse(&words, &|key| std::env::var(key).ok()) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("voice-probe: {error}\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    match execute(command).await {
        Ok(text) => {
            if write_stdout(&text).is_err() {
                eprintln!("voice-probe: cannot write to stdout");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("voice-probe: {error}");
            std::process::exit(1);
        }
    }
}
