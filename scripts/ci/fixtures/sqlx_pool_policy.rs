//! Compile-only input for the check lane. The functions are never executed.

use sqlx::postgres::{PgConnectOptions, PgConnection, PgPoolOptions};
use sqlx::{ConnectOptions, Connection, PgPool as Alias, Pool, Postgres};
use two_bot_core::database_tls::TlsPolicy;
use two_bot_store::pool::{connect_lazy_pool_with_tls, connect_pool_with_tls};

fn raw_constructors(url: &str, options: PgConnectOptions) {
    drop(Alias::connect(url)); // disallowed
    drop(Pool::<Postgres>::connect_with(options.clone())); // disallowed
    let _ = Alias::connect_lazy(url); // disallowed
    let _ = Alias::connect_lazy_with(options.clone()); // disallowed
    drop(PgPoolOptions::new().connect(url)); // disallowed
    drop(PgPoolOptions::new().connect_with(options.clone())); // disallowed
    let _ = PgPoolOptions::new().connect_lazy(url); // disallowed
    let _ = PgPoolOptions::new().connect_lazy_with(options.clone()); // disallowed
    drop(PgConnection::connect(url)); // disallowed
    drop(PgConnection::connect_with(&options)); // disallowed
    drop(options.connect()); // disallowed
}

fn fenced_constructors(url: &str) {
    drop(connect_pool_with_tls(url, TlsPolicy::LocalOnly));
    let _ = connect_lazy_pool_with_tls(url, TlsPolicy::LocalOnly, 2);
}

#[test]
fn typecheck_without_connecting() {
    let _: fn(&str, PgConnectOptions) = raw_constructors;
    let _: fn(&str) = fenced_constructors;
}
