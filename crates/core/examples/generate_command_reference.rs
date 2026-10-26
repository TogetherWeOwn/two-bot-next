// Reuse the canonical renderer and its metadata, without running the test writer.
include!("../tests/reference_docs.rs");

fn generate_command_reference() -> Result<&'static str, &'static str> {
    let root = repository_root();
    let [(command_path, commands), (configuration_path, configuration)] = generated_documents();
    let saved_configuration = std::fs::read_to_string(root.join(configuration_path))
        .map_err(|_| "configuration_reference_unreadable")?;
    check_document(configuration_path, &saved_configuration, &configuration)
        .map_err(|_| "configuration_reference_stale")?;

    // Validate the tracked reference before creating or changing any output.
    std::fs::create_dir_all(root.join("docs/generated"))
        .map_err(|_| "command_reference_directory_unwritable")?;
    std::fs::write(root.join(command_path), commands)
        .map_err(|_| "command_reference_unwritable")?;
    Ok(command_path)
}

fn main() -> std::process::ExitCode {
    match generate_command_reference() {
        Ok(path) => {
            println!(
                "{}",
                json!({"schema": "command_reference.v1", "status": "generated", "path": path})
            );
            std::process::ExitCode::SUCCESS
        }
        Err(code) => {
            let next_action = if code.starts_with("configuration_reference_") {
                "Review and explicitly regenerate the tracked configuration reference; then retry."
            } else {
                "Make the generated output directory writable; then retry."
            };
            println!(
                "{}",
                json!({"schema": "command_reference.v1", "status": "error", "code": code, "retryable": false, "next_action": next_action})
            );
            std::process::ExitCode::FAILURE
        }
    }
}
