//! `medatat` — the CLI driver for the API.
//!
//! Three roles: a curl-style gateway for smoke tests and manual poking, a `seed` command
//! that writes a synthetic corpus to disk, and a `push` command that drives one into a
//! running Worker through the ordinary write path. Sharing `medatat-core`'s wire types means the CLI and the
//! desktop app cannot drift.
//!
//! See `docs/09-SETUP.md` and `docs/07-TESTING.md`.

mod dev;
mod fetch;
mod push;
mod seed;

use anyhow::{Context, Result};
use incurs::cli::Cli;
use incurs::fetch::FetchGatewayOptions;

const DEFAULT_API: &str = "http://localhost:8787";

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // `seed` is a local operation, not an API call, so it is handled before the gateway.
    if argv.first().map(String::as_str) == Some("seed") {
        return seed::run(&argv[1..]);
    }

    // `push` drives the ordinary API in a loop rather than issuing one call, so it is its
    // own command rather than a gateway path.
    if argv.first().map(String::as_str) == Some("push") {
        return push::run(&argv[1..]).await;
    }

    // `dev` reads the local emulator's state directly. It never talks to the Worker —
    // see the module docs for why there is no endpoint behind it.
    if argv.first().map(String::as_str) == Some("dev") {
        return dev::run(&argv[1..]);
    }

    let api = std::env::var("MEDATAT_API").unwrap_or_else(|_| DEFAULT_API.to_string());
    let token = std::env::var("MEDATAT_TOKEN").ok();

    let cli = Cli::create("medatat").fetch_gateway(
        "api",
        fetch::HttpFetch::new(&api, token),
        FetchGatewayOptions {
            description: Some(format!("medatat API gateway ({api})")),
            base_path: None,
            output_policy: None,
        },
    );

    cli.serve()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("CLI failed")
}
