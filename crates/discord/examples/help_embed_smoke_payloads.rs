//! Emit the help-embed smoke payloads for scripts/staging_help_embed_smoke.py.
//!
//! Usage: `help_embed_smoke_payloads [OUTPUT_PATH]`
//! With no path the payloads go to stdout; with a path they are written
//! there and stdout carries one status line, like the command-reference
//! generator. No token, network or environment input is involved.

use std::io::Write as _;

fn report(status: &str, code: Option<&str>) -> String {
    let mut line = String::from("{\"schema\":\"help_embed_smoke.v1\",\"status\":\"");
    line.push_str(status);
    line.push('"');
    if let Some(code) = code {
        line.push_str(",\"code\":\"");
        line.push_str(code);
        line.push('"');
    }
    line.push_str("}\n");
    line
}

fn emit() -> Result<(), &'static str> {
    let payloads = two_bot_discord::help_embed_smoke::help_embed_smoke_payloads()?;
    let bytes = serde_json::to_vec(&payloads).map_err(|_| "payload_encode_failed")?;
    let output = std::env::args().nth(1);
    match output {
        Some(path) => {
            std::fs::write(path, &bytes).map_err(|_| "payload_write_failed")?;
            std::io::stdout()
                .write_all(report("generated", None).as_bytes())
                .map_err(|_| "payload_write_failed")
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(&bytes)
                .and_then(|()| stdout.write_all(b"\n"))
                .map_err(|_| "payload_write_failed")
        }
    }
}

fn main() -> std::process::ExitCode {
    match emit() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(code) => {
            let _ = std::io::stdout().write_all(report("error", Some(code)).as_bytes());
            std::process::ExitCode::FAILURE
        }
    }
}
