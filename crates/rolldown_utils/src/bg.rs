//! perf/bun-gap investigation switches (`RD_BG_*` env vars). Not for production use.

use std::sync::OnceLock;
use std::time::Instant;

/// `bg_flag!("RD_BG_X")` is true when the env var is set to anything but `0`. Cached per call site.
#[macro_export]
macro_rules! bg_flag {
  ($name:literal) => {{
    static F: ::std::sync::OnceLock<bool> = ::std::sync::OnceLock::new();
    *F.get_or_init(|| ::std::env::var_os($name).is_some_and(|v| v != "0"))
  }};
}

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since the first call (call it once at build start).
pub fn now_ms() -> f64 {
  EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

pub fn timing() -> bool {
  bg_flag!("RD_BG_TIMING")
}

#[expect(clippy::print_stderr, reason = "perf investigation output")]
pub fn log(msg: std::fmt::Arguments<'_>) {
  eprintln!("[bg {:9.3}] {msg}", now_ms());
}
