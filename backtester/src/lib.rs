//! kalshi_backtester — event-driven tick-level backtester for Kalshi orderbook-delta data.
//!
//! Pipeline: data loader -> MarketEvent stream -> OrderBook reconstruction -> Engine
//! -> Strategy (via Ctx) -> execution/fees -> Portfolio/metrics -> Report (infra-compatible).

pub mod types;
pub mod strategy;

pub mod config;
pub mod runspec;
pub mod orderbook;
pub mod fees;
pub mod latency;
pub mod slippage;
pub mod rewards;
pub mod execution;
pub mod portfolio;
pub mod settlement;
pub mod metrics;
pub mod fit_logistic;
pub mod event_curves;
pub mod engine;
pub mod report;
pub mod exports;
pub mod optimize;

pub mod data;
pub mod adapters;
pub mod strategies;

pub use strategy::{Ctx, Strategy};
pub use types::*;
