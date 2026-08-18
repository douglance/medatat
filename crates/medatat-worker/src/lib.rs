//! The medatat Cloudflare Worker.
//!
//! Written in Rust so `medatat-core` is shared verbatim with the desktop client — one
//! `validate` engine, one `FieldKind`→column mapping, one set of time and decimal
//! semantics. See `docs/adr/0004-workers-rs.md`.
//!
//! Layout:
//!
//! - [`logic`] and [`http`] — every decision and every status code, as pure functions.
//!   Tests natively.
//! - [`store`], [`routes`], [`case_do`] — thin adapters over D1, KV, and Durable Objects.
//!   WASM only.
//! - [`mail`] — the `Mailer` seam.

pub mod error;
pub mod http;
pub mod logic;
pub mod mail;

#[cfg(target_arch = "wasm32")]
pub mod case_do;
#[cfg(target_arch = "wasm32")]
pub mod routes;
#[cfg(target_arch = "wasm32")]
pub mod store;

#[cfg(target_arch = "wasm32")]
pub use case_do::CaseDO;

#[cfg(target_arch = "wasm32")]
#[worker::event(fetch)]
pub async fn fetch(
    req: worker::Request,
    env: worker::Env,
    _ctx: worker::Context,
) -> worker::Result<worker::Response> {
    worker::Router::new()
        // --------------------------------------------------------------- auth
        .post_async("/auth/request", routes::auth::request_code)
        .post_async("/auth/verify", routes::auth::verify)
        .post_async("/auth/logout", routes::auth::logout)
        .get_async("/auth/me", routes::auth::me)
        // ------------------------------------------------------------- config
        .get_async("/config", routes::config::get_config)
        .post_async("/config/forms", routes::config::create_form)
        .patch_async("/config/forms/:form_id", routes::config::patch_form)
        .post_async(
            "/config/forms/:form_id/sections",
            routes::config::create_section,
        )
        .patch_async(
            "/config/sections/:section_id",
            routes::config::patch_section,
        )
        .delete_async(
            "/config/sections/:section_id",
            routes::config::delete_section,
        )
        .post_async("/config/fields", routes::config::create_field)
        .patch_async("/config/fields/:field_id", routes::config::patch_field)
        .post_async(
            "/config/sections/:section_id/fields",
            routes::config::place_field,
        )
        .patch_async(
            "/config/sections/:section_id/fields/:field_id",
            routes::config::patch_placement,
        )
        .delete_async(
            "/config/sections/:section_id/fields/:field_id",
            routes::config::unplace_field,
        )
        // -------------------------------------------------------------- cases
        .get_async("/cases", routes::cases::list_cases)
        .post_async("/cases", routes::cases::create_case)
        .get_async("/cases/:case_id/values", routes::cases::get_values)
        .post_async("/cases/:case_id/values", routes::cases::put_values)
        // --------------------------------------------------------------- bulk
        .post_async("/bulk/cases", routes::bulk::create_cases)
        .get_async("/bulk/export", routes::bulk::export)
        .post_async("/admin/reindex", routes::bulk::reindex)
        // ------------------------------------------------------------- health
        .get_async("/health", health)
        .run(req, env)
        .await
}

/// No auth, by design: this is what a load balancer and a smoke test call.
#[cfg(target_arch = "wasm32")]
async fn health(
    _req: worker::Request,
    ctx: worker::RouteContext<()>,
) -> worker::Result<worker::Response> {
    let config_rev = match routes::config::current_rev(&ctx.env).await {
        Ok(rev) => rev,
        Err(e) => return routes::fail(e),
    };
    routes::json(
        medatat_core::wire::Health {
            version: env!("CARGO_PKG_VERSION").to_string(),
            config_rev,
        },
        200,
    )
}
