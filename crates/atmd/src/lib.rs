//! ATM Daemon - Session registry and broadcast server
//!
//! This crate provides the core infrastructure for the ATM daemon:
//! - `registry` - Session registry actor for tracking coding-agent sessions
//! - `server` - Unix socket server for client connections
//! - `monitor` - Process monitoring for CPU/memory tracking
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    atmd daemon                         │
//! ├─────────────────────────────────────────────────────────────┤
//! │                                                             │
//! │  ┌─────────────────┐     ┌─────────────────────────────┐   │
//! │  │  DaemonServer   │────▶│     RegistryActor           │   │
//! │  │ (Unix Socket)   │     │  (session state owner)      │   │
//! │  └────────┬────────┘     └──────────────┬──────────────┘   │
//! │           │                             │                   │
//! │           │ connections                 │ events            │
//! │           ▼                             ▼                   │
//! │  ┌─────────────────┐     ┌─────────────────────────────┐   │
//! │  │ConnectionHandler│     │   broadcast::Sender         │   │
//! │  │  (per client)   │     │   (event distribution)      │   │
//! │  └─────────────────┘     └─────────────────────────────┘   │
//! │                                                             │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Panic-Free Guarantees
//!
//! All production code in this crate follows the panic-free policy from CLAUDE.md:
//! - No `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, `todo!()`
//! - All fallible operations return `Result` or `Option`
//! - Channel operations handle closure gracefully

pub mod discovery;
pub mod monitor;
pub mod project;
pub mod registry;
pub mod server;
pub mod tmux;
