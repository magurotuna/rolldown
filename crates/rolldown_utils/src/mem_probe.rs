//! Memory marks for the peak RSS investigation. Not for upstream.
//!
//! When `ROLLDOWN_MEM_PROBE=<path>` is set, [`mark`] appends one JSON line to `<path>` with the
//! time, the mark name, the process RSS and HWM, and the output of the allocator probe that the
//! embedder registered with [`set_alloc_probe`]. Without the variable, [`mark`] only reads a
//! `OnceLock`.
//!
//! - `ROLLDOWN_MEM_PROBE_PAGES=all|<prefix>,<prefix>`: for the matching marks, the allocator probe
//!   also reports the statistics of every allocator heap.
//! - `ROLLDOWN_MEM_PROBE_SMAPS=1`: adds `AnonHugePages` and `Anonymous` from
//!   `/proc/self/smaps_rollup` (Linux only).
use std::{
  io::Write as _,
  sync::{Mutex, OnceLock},
  time::{SystemTime, UNIX_EPOCH},
};

struct Config {
  file: Mutex<std::fs::File>,
  pages: Option<Vec<String>>,
  smaps: bool,
}

static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
static ALLOC_PROBE: OnceLock<fn(bool) -> String> = OnceLock::new();
static MARK_HOOK: OnceLock<fn(&str)> = OnceLock::new();

fn config() -> Option<&'static Config> {
  CONFIG
    .get_or_init(|| {
      let path = std::env::var_os("ROLLDOWN_MEM_PROBE")?;
      let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
      let pages = std::env::var("ROLLDOWN_MEM_PROBE_PAGES")
        .ok()
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
      let smaps = std::env::var_os("ROLLDOWN_MEM_PROBE_SMAPS").is_some();
      Some(Config { file: Mutex::new(file), pages, smaps })
    })
    .as_ref()
}

/// Whether `ROLLDOWN_MEM_PROBE` is set.
pub fn enabled() -> bool {
  config().is_some()
}

/// Registers the allocator probe. The argument tells it whether to walk the heap pages. It returns
/// a JSON value.
pub fn set_alloc_probe(probe: fn(bool) -> String) {
  let _ = ALLOC_PROBE.set(probe);
}

fn status_kb(text: &str, key: &str) -> u64 {
  text
    .lines()
    .find(|l| l.starts_with(key))
    .and_then(|l| l.split_whitespace().nth(1))
    .and_then(|v| v.parse().ok())
    .unwrap_or(0)
}

/// Registers a function that runs at every mark, after the mark is written.
pub fn set_mark_hook(hook: fn(&str)) {
  let _ = MARK_HOOK.set(hook);
}

/// Appends a mark. See the module documentation.
pub fn mark(name: &str) {
  let Some(cfg) = config() else { return };
  let t_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
  let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
  let mut line = format!(
    "{{\"t\":{t_ms},\"name\":\"{name}\",\"thread\":\"{}\",\"rss_kb\":{},\"hwm_kb\":{}",
    std::thread::current().name().unwrap_or("?"),
    status_kb(&status, "VmRSS:"),
    status_kb(&status, "VmHWM:")
  );
  if cfg.smaps {
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    line.push_str(&format!(
      ",\"anon_kb\":{},\"anon_huge_kb\":{}",
      status_kb(&rollup, "Anonymous:"),
      status_kb(&rollup, "AnonHugePages:")
    ));
  }
  if let Some(probe) = ALLOC_PROBE.get() {
    let walk = cfg
      .pages
      .as_ref()
      .is_some_and(|p| p.iter().any(|prefix| prefix == "all" || name.starts_with(prefix.as_str())));
    let start = std::time::Instant::now();
    let alloc = probe(walk);
    line.push_str(&format!(",\"alloc\":{alloc},\"probe_ms\":{}", start.elapsed().as_millis()));
  }
  line.push_str("}\n");
  if let Ok(mut file) = cfg.file.lock() {
    let _ = file.write_all(line.as_bytes());
  }
  if let Some(hook) = MARK_HOOK.get() {
    hook(name);
  }
}
