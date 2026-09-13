//! # nanus-adapter-config
//!
//! The configuration adapter: one schema, one TOML file, one precedence order.
//!
//! ## Precedence
//!
//! [`NanusConfig::load`] resolves its source in this order, first match winning:
//!
//! | Level | Source | Notes |
//! |---|---|---|
//! | 1 | the explicit path argument | `None` falls through to the next level |
//! | 2 | `$NANUS_CONFIG` | set but empty falls through |
//! | 3 | `<config dir>/nanus/config.toml` | `etcetera`'s platform directory |
//! | 4 | the built-in defaults | reached only when the level-3 file is absent |
//!
//! A **missing** file at level 1–3 is not an error: it yields
//! [`NanusConfig::default`]. A **malformed** file is always an error, because
//! silence is for absence and never for damage. A file whose `config_version` is
//! newer than [`CONFIG_VERSION`] is refused outright rather than partially read.
//!
//! ## The API key is not configuration
//!
//! The provider key comes from `$DEEPSEEK_API_KEY` via [`api_key`] and is never a
//! field of [`NanusConfig`]. This crate writes configuration to disk and has no
//! code path that could write a key: the type the writer serialises has nowhere
//! to put one, and a file that contains one is loaded with that key ignored. The
//! `Debug` rendering of a loaded configuration therefore cannot contain a secret.
//!
//! ## Migrations
//!
//! The schema carries [`CONFIG_VERSION`] and a real migration chain, applied
//! through [`nanus_kernel::run_startup_migrations`] before the document is
//! deserialised. Version 0 files used `max_output_tokens`; the 0 → 1 step renames
//! it to `max_tokens`. Running migrations on the raw document — rather than on the
//! typed struct — is what lets a genuinely old shape be read at all.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// A `pub` item inside a private module is reachable only through this crate's own
// re-exports. The lint cannot tell that from an orphaned item, and every such item
// here is deliberate.
#![allow(unreachable_pub)]

mod config;
mod error;

pub use config::{
    API_KEY_ENV, CONFIG_ENV, CONFIG_VERSION, DEFAULT_MAX_PARALLEL_TOOLS,
    DEFAULT_MAX_STEPS_PER_TURN, DEFAULT_MAX_TOKENS, DEFAULT_MODEL, NanusConfig, ReasoningEffort,
    TuiDetail, api_key,
};
pub use error::ConfigError;
