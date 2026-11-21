//! Allocator probe for `rolldown_utils::mem_probe`. Not for upstream.
//!
//! Reports `mi_process_info` (current and peak commit of mimalloc), the live bytes of the tracking
//! allocator (when built with `tracking_allocator`), and, when asked, the statistics of every
//! mimalloc heap in the main subprocess (`mi_subproc_visit_heaps` + `mi_heap_stats_get_json`). The
//! statistics of a thread reach its heap after 4 MiB of churn, so they lag behind.
//!
//! `ROLLDOWN_MEM_SAMPLER=<path>[:<ms>]` starts a thread that writes `t_ms,rss_kb,mi_commit` every
//! `<ms>` (default 20) milliseconds.
use std::{
  ffi::{c_char, c_void},
  fmt::Write as _,
  time::{SystemTime, UNIX_EPOCH},
};

#[repr(C)]
#[derive(Clone, Copy)]
struct MiSubprocId {
  _id: *mut c_void,
}

type MiHeapVisit = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;

unsafe extern "C" {
  fn mi_heap_stats_get_json(heap: *mut c_void, buf_size: usize, buf: *mut c_char) -> *mut c_char;
  fn mi_subproc_main() -> MiSubprocId;
  fn mi_subproc_visit_heaps(subproc: MiSubprocId, visitor: MiHeapVisit, arg: *mut c_void) -> bool;
  fn mi_heap_main() -> *mut c_void;
  fn mi_process_info(
    elapsed_msecs: *mut usize,
    user_msecs: *mut usize,
    system_msecs: *mut usize,
    current_rss: *mut usize,
    peak_rss: *mut usize,
    current_commit: *mut usize,
    peak_commit: *mut usize,
    page_faults: *mut usize,
  );
}

/// Size of the buffer for one `mi_heap_stats_get_json` result. The JSON has about 230 lines.
const STATS_BUF: usize = 64 << 10;

struct Heaps {
  main: *mut c_void,
  buf: Vec<u8>,
  out: String,
}

/// Appends the statistics of one heap. `mi_heap_stats_get_json` only reads the counters of the heap
/// (and merges the statistics of the calling thread into them), so it is safe while other threads
/// allocate. Do not use `mi_heap_visit_blocks` here: it collects the free lists of pages that other
/// threads own, which corrupts the heap (`arena-page.c`, `mi_heap_visit_page`).
unsafe extern "C" fn visit_heap(heap: *mut c_void, arg: *mut c_void) -> bool {
  // SAFETY: `arg` is our `Heaps`.
  let heaps = unsafe { &mut *arg.cast::<Heaps>() };
  heaps.buf.fill(0);
  // SAFETY: `heap` is a live heap that mimalloc passes to the visitor; the buffer is writable.
  let json =
    unsafe { mi_heap_stats_get_json(heap, heaps.buf.len(), heaps.buf.as_mut_ptr().cast()) };
  if json.is_null() {
    return true;
  }
  let len = heaps.buf.iter().position(|&b| b == 0).unwrap_or(heaps.buf.len());
  let text = String::from_utf8_lossy(&heaps.buf[..len]);
  let label = if heap == heaps.main { "main".to_string() } else { format!("{heap:p}") };
  if !heaps.out.is_empty() {
    heaps.out.push(',');
  }
  let _ = write!(heaps.out, "\"{label}\":");
  heaps.out.extend(text.chars().filter(|c| !c.is_whitespace()));
  true
}

fn commit() -> (usize, usize) {
  let (mut e, mut u, mut s, mut rss, mut prss, mut c, mut pc, mut f) = (0, 0, 0, 0, 0, 0, 0, 0);
  // SAFETY: all out pointers are valid.
  unsafe { mi_process_info(&mut e, &mut u, &mut s, &mut rss, &mut prss, &mut c, &mut pc, &mut f) };
  (c, pc)
}

fn probe(walk: bool) -> String {
  let (c, pc) = commit();
  #[cfg(feature = "tracking_allocator")]
  let live = rolldown_tracking_allocator::stats().live_bytes;
  #[cfg(not(feature = "tracking_allocator"))]
  let live = 0usize;
  let mut out = format!("{{\"mi_commit\":{c},\"mi_peak_commit\":{pc},\"live\":{live}");
  if walk {
    // SAFETY: `mi_heap_main` has no preconditions.
    let main = unsafe { mi_heap_main() };
    let mut heaps = Heaps { main, buf: vec![0; STATS_BUF], out: String::new() };
    // SAFETY: the visitor only reads the heap statistics; `heaps` outlives the call.
    unsafe { mi_subproc_visit_heaps(mi_subproc_main(), visit_heap, (&raw mut heaps).cast()) };
    let _ = write!(out, ",\"heaps\":{{{}}}", heaps.out);
  }
  out.push('}');
  out
}

pub fn init() {
  rolldown_utils::mem_probe::set_alloc_probe(probe);
  #[cfg(not(feature = "tracking_allocator"))]
  rolldown_utils::mem_probe::set_mark_hook(crate::big_alloc::on_mark);
  #[cfg(not(feature = "tracking_allocator"))]
  let big_peak = std::env::var("ROLLDOWN_MEM_BIG_PEAK_GIB")
    .ok()
    .and_then(|v| v.parse::<f64>().ok())
    .map(|g| (g * 1048576.0) as u64);
  let Ok(spec) = std::env::var("ROLLDOWN_MEM_SAMPLER") else { return };
  let (path, ms) = match spec.rsplit_once(':') {
    Some((p, m)) if m.parse::<u64>().is_ok() => (p.to_string(), m.parse::<u64>().unwrap_or(20)),
    _ => (spec, 20),
  };
  let _ = std::thread::Builder::new().name("rolldown-mem-sampler".into()).spawn(move || {
    use std::io::Write as _;
    let Ok(mut file) = std::fs::File::create(&path) else { return };
    let _ = writeln!(file, "t_ms,rss_kb,mi_commit");
    let mut last_dump_kb = 0u64;
    loop {
      let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
      let status = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
      let rss_pages: u64 =
        status.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
      let _ = writeln!(file, "{t},{},{}", rss_pages * 4, commit().0);
      #[cfg(not(feature = "tracking_allocator"))]
      if let Some(min_kb) = big_peak {
        let rss_kb = rss_pages * 4;
        if rss_kb >= min_kb && rss_kb >= last_dump_kb + 262_144 {
          last_dump_kb = rss_kb;
          crate::big_alloc::dump(&format!("rss_kb={rss_kb}"));
        }
      }
      std::thread::sleep(std::time::Duration::from_millis(ms));
    }
  });
}
