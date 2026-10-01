//! Offline cutover only. Never migrates schemas or calls Discord.
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use two_bot_cutover::legacy_copy::{copy, mapping::GROUPS, options::Options};

const HELP: &str = "legacy_copy [--plan] [--groups ready|all|group,...] [--batch-size 1..10000]
  [--source-url URL] [--target-url URL] [--apply] [--allow-live-target]

Dry-run is the default. --plan prints mappings/pending groups without connecting.
Prefer LEGACY_COPY_SOURCE_URL and LEGACY_COPY_TARGET_URL to keep URLs out of argv.
No fallback to app credentials. Source/target must be separate databases.
Unknown targets (including staging) require --allow-live-target AND separate
cutover approval. Stop both bot writers; pre-apply schemas separately.
Pending groups refuse before any connection. No schema creation or data deletion.";

#[tokio::main]
async fn main() {
    std::process::exit(run().await);
}

async fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match Options::parse(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    if options.help {
        println!("{HELP}");
        return 0;
    }
    let plan = serde_json::json!({
        "version": 1,
        "source_revision": "96777468472f23a02a1e97a43ffab3912fe5df2a",
        "groups": GROUPS,
        "selection": options.groups,
        "apply": options.apply,
        "batch_size": options.batch_size,
    });
    println!("{}", serde_json::to_string_pretty(&plan).unwrap());
    if options.plan {
        return 0;
    }
    let tables = match options.tables() {
        Ok(tables) => tables,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    let (source_options, target_options) = match options.connections() {
        Ok(connections) => connections,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    let source = match PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(source_options.options([
            ("default_transaction_read_only", "on"),
            ("statement_timeout", "60000"),
            ("timezone", "UTC"),
        ]))
        .await
    {
        Ok(pool) => pool,
        Err(_) => {
            eprintln!("source connection failed (details withheld)");
            return 1;
        }
    };
    let target = match PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(target_options.options([
            (
                "default_transaction_read_only",
                if options.apply { "off" } else { "on" },
            ),
            ("statement_timeout", "60000"),
            ("timezone", "UTC"),
        ]))
        .await
    {
        Ok(pool) => pool,
        Err(_) => {
            eprintln!("target connection failed (details withheld)");
            source.close().await;
            return 1;
        }
    };
    let result = copy(&source, &target, &tables, options.batch_size, options.apply).await;
    source.close().await;
    target.close().await;
    match result {
        Ok(receipts) => {
            println!("{}", serde_json::to_string_pretty(&receipts).unwrap());
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}
