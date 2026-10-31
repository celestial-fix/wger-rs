// `Response` is this crate's error type by design (see `httpx::detail`), and
// it is large enough to trip clippy's 128-byte heuristic on the private
// helpers that return it directly.
#![allow(clippy::result_large_err)]

pub mod api;
pub mod auth;
pub mod config;
pub mod engine;
pub mod forwarded;
pub mod healthcheck;
pub mod httpx;
pub mod jsonutil;
pub mod proxy;
pub mod state;
pub mod static_files;
pub mod timefmt;
