//! Binding adapters. Every module here is WASM-only; the rules they enforce live in
//! `logic/`, which tests natively.

pub mod case_sql;
pub mod d1;
pub mod kv;
