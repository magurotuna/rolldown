//! Allocator probe for `rolldown_utils::mem_probe`. Not for upstream.
//!
//! Reports `mi_process_info` (current and peak commit of mimalloc), the live bytes of the tracking
//! allocator (when built with `tracking_allocator`), and, when asked, a walk of the pages of every
//! mimalloc heap in the main subprocess (`mi_subproc_visit_heaps` + `mi_heap_visit_blocks`). The
//! walk does not stop other threads, so its numbers are approximate.
//!
//! `ROLLDOWN_MEM_SAMPLER=<path>[:<ms>]` starts a thread that writes `t_ms,rss_kb,mi_commit` every
//! `<ms>` (default 20) milliseconds.
use std::{
  ffi::c_void,
  fmt::Write as _,
  time::{SystemTime, UNIX_EPOCH},
};

#[repr(C)]
struct MiHeapArea {
  blocks: *mut c_void,
  reserved: usize,
  committed: usize,
  used: usize,
  block_size: usize,
  full_block_size: usize,
  reserved1: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MiSubprocId {
  _id: *mut c_void,
}

type MiBlockVisit =
  unsafe extern "C" fn(*const c_void, *const MiHeapArea, *mut c_void, usize, *mut c_void) -> bool;
type MiHeapVisit = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;

unsafe extern "C" {
  fn mi_heap_visit_blocks(
    heap: *mut c_void,
    visit_blocks: bool,
    visitor: MiBlockVisit,
    arg: *mut c_void,
  ) -> bool;
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

const BUCKET_LIMITS: [usize; 6] = [1 << 10, 8 << 10, 64 << 10, 512 << 10, 4 << 20, usize::MAX];
const BUCKET_NAMES: [&str; 6] = ["le1K", "le8K", "le64K", "le512K", "le4M", "gt4M"];

#[derive(Default)]
struct HeapAcc {
  pages: [u64; 6],
  reserved: [u64; 6],
  committed: [u64; 6],
  used: [u64; 6],
  /// Occupancy deciles (used / committed), weighted by the committed bytes.
  hist: [u64; 10],
}

unsafe extern "C" fn visit_area(
  _heap: *const c_void,
  area: *const MiHeapArea,
  block: *mut c_void,
  _block_size: usize,
  arg: *mut c_void,
) -> bool {
  if !block.is_null() || area.is_null() {
    return true;
  }
  // SAFETY: mimalloc passes a valid area, and `arg` is our `HeapAcc`.
  let (a, acc) = unsafe { (&*area, &mut *arg.cast::<HeapAcc>()) };
  let used = (a.used * a.full_block_size) as u64;
  let b = BUCKET_LIMITS.iter().position(|&l| a.full_block_size <= l).unwrap_or(5);
  acc.pages[b] += 1;
  acc.reserved[b] += a.reserved as u64;
  acc.committed[b] += a.committed as u64;
  acc.used[b] += used;
  if a.committed > 0 {
    let d = ((used * 10) / a.committed as u64).min(9) as usize;
    acc.hist[d] += a.committed as u64;
  }
  true
}

struct Heaps {
  main: *mut c_void,
  out: String,
}

unsafe extern "C" fn visit_heap(heap: *mut c_void, arg: *mut c_void) -> bool {
  // SAFETY: `arg` is our `Heaps`.
  let heaps = unsafe { &mut *arg.cast::<Heaps>() };
  let mut acc = HeapAcc::default();
  // SAFETY: `heap` is a live heap that mimalloc passes to the visitor.
  unsafe { mi_heap_visit_blocks(heap, false, visit_area, (&raw mut acc).cast()) };
  let label = if heap == heaps.main { "main".to_string() } else { format!("{heap:p}") };
  if !heaps.out.is_empty() {
    heaps.out.push(',');
  }
  let _ = write!(heaps.out, "\"{label}\":{{");
  for i in 0..6 {
    let _ = write!(
      heaps.out,
      "{}\"{}\":[{},{},{},{}]",
      if i > 0 { "," } else { "" },
      BUCKET_NAMES[i],
      acc.pages[i],
      acc.reserved[i],
      acc.committed[i],
      acc.used[i]
    );
  }
  let _ = write!(heaps.out, ",\"hist\":{:?}}}", acc.hist);
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
    let mut heaps = Heaps { main: unsafe { mi_heap_main() }, out: String::new() };
    // SAFETY: the visitor only reads the heaps; `heaps` outlives the call.
    unsafe { mi_subproc_visit_heaps(mi_subproc_main(), visit_heap, (&raw mut heaps).cast()) };
    let _ = write!(out, ",\"heaps\":{{{}}}", heaps.out);
  }
  out.push('}');
  out
}

pub fn init() {
  rolldown_utils::mem_probe::set_alloc_probe(probe);
  let Ok(spec) = std::env::var("ROLLDOWN_MEM_SAMPLER") else { return };
  let (path, ms) = match spec.rsplit_once(':') {
    Some((p, m)) if m.parse::<u64>().is_ok() => (p.to_string(), m.parse::<u64>().unwrap_or(20)),
    _ => (spec, 20),
  };
  let _ = std::thread::Builder::new().name("rolldown-mem-sampler".into()).spawn(move || {
    use std::io::Write as _;
    let Ok(mut file) = std::fs::File::create(&path) else { return };
    let _ = writeln!(file, "t_ms,rss_kb,mi_commit");
    loop {
      let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
      let status = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
      let rss_pages: u64 =
        status.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
      let _ = writeln!(file, "{t},{},{}", rss_pages * 4, commit().0);
      std::thread::sleep(std::time::Duration::from_millis(ms));
    }
  });
}
