//! Tracker of live big allocations for the peak RSS investigation. Not for upstream.
//!
//! The global allocator in probe builds is [`BigAllocTracker`]: mimalloc, plus a table of the
//! live allocations of at least `ROLLDOWN_MEM_BIG_MIN` bytes (default 1 MiB), each with the return
//! addresses of its call stack (glibc `backtrace`). Without `ROLLDOWN_MEM_BIG=<path>`, the tracker
//! only loads one atomic flag per allocation of that size.
//!
//! [`dump`] appends the table, grouped by call stack, to `<path>`. Each group is one line:
//! `<tag> <bytes> <count> <frame> <frame> ...`, where a frame inside this library is `b+0x<offset>`
//! (symbolize with `addr2line -e <binding.node>`), and any other frame is `0x<address>`.
//!
//! - Marks whose names start with a prefix in `ROLLDOWN_MEM_BIG_MARKS=<prefix>,...` dump the table.
//! - The memory sampler (`ROLLDOWN_MEM_SAMPLER`) also dumps it each time that the RSS is 256 MiB
//!   above the last dump and above `ROLLDOWN_MEM_BIG_PEAK_GIB` GiB, so the last such dump is near
//!   the peak.
use std::{
  alloc::{GlobalAlloc, Layout},
  cell::Cell,
  collections::HashMap,
  ffi::{c_int, c_void},
  io::Write as _,
  sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
  },
};

const FRAMES: usize = 20;

unsafe extern "C" {
  fn backtrace(buffer: *mut *mut c_void, size: c_int) -> c_int;
}

#[repr(C)]
struct DlInfo {
  dli_fname: *const i8,
  dli_fbase: *mut c_void,
  dli_sname: *const i8,
  dli_saddr: *mut c_void,
}

unsafe extern "C" {
  fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static MIN: AtomicUsize = AtomicUsize::new(usize::MAX);
static TABLE: Mutex<Option<HashMap<usize, (usize, [usize; FRAMES])>>> = Mutex::new(None);
static PATH: OnceLock<String> = OnceLock::new();
static MARKS: OnceLock<Vec<String>> = OnceLock::new();
static BASE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
  /// Set while this thread is inside the tracker, so that the allocations of the tracker itself go
  /// to mimalloc without tracking.
  static BUSY: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` unless this thread is already inside the tracker (or its thread-local storage is gone).
fn guarded(f: impl FnOnce()) {
  let entered = BUSY.try_with(|b| if b.get() { false } else { b.set(true); true }).unwrap_or(false);
  if entered {
    f();
    let _ = BUSY.try_with(|b| b.set(false));
  }
}

fn record(ptr: *mut u8, size: usize) {
  guarded(|| {
    let mut frames = [std::ptr::null_mut::<c_void>(); FRAMES];
    // SAFETY: the buffer has `FRAMES` entries.
    let n = unsafe { backtrace(frames.as_mut_ptr(), FRAMES as c_int) }.max(0) as usize;
    let mut ips = [0usize; FRAMES];
    for (ip, f) in ips.iter_mut().zip(&frames[..n]) {
      *ip = *f as usize;
    }
    if let Ok(mut t) = TABLE.lock() {
      t.get_or_insert_with(HashMap::default).insert(ptr as usize, (size, ips));
    }
  });
}

fn forget(ptr: *mut u8) {
  guarded(|| {
    if let Ok(mut t) = TABLE.lock()
      && let Some(t) = t.as_mut()
    {
      t.remove(&(ptr as usize));
    }
  });
}

pub struct BigAllocTracker;

// SAFETY: every call goes to mimalloc; the tracker only keeps side records.
unsafe impl GlobalAlloc for BigAllocTracker {
  #[inline]
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    // SAFETY: forwarded.
    let p = unsafe { mimalloc_safe::MiMalloc.alloc(layout) };
    if layout.size() >= MIN.load(Ordering::Relaxed) && !p.is_null() {
      record(p, layout.size());
    }
    p
  }

  #[inline]
  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    // SAFETY: forwarded.
    let p = unsafe { mimalloc_safe::MiMalloc.alloc_zeroed(layout) };
    if layout.size() >= MIN.load(Ordering::Relaxed) && !p.is_null() {
      record(p, layout.size());
    }
    p
  }

  #[inline]
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    if layout.size() >= MIN.load(Ordering::Relaxed) {
      forget(ptr);
    }
    // SAFETY: forwarded.
    unsafe { mimalloc_safe::MiMalloc.dealloc(ptr, layout) }
  }

  #[inline]
  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let min = MIN.load(Ordering::Relaxed);
    if layout.size() >= min {
      forget(ptr);
    }
    // SAFETY: forwarded.
    let p = unsafe { mimalloc_safe::MiMalloc.realloc(ptr, layout, new_size) };
    if new_size >= min && !p.is_null() {
      record(p, new_size);
    } else if p.is_null() && layout.size() >= min {
      record(ptr, layout.size());
    }
    p
  }
}

pub fn init() {
  let Ok(path) = std::env::var("ROLLDOWN_MEM_BIG") else { return };
  let min = std::env::var("ROLLDOWN_MEM_BIG_MIN").ok().and_then(|v| v.parse().ok()).unwrap_or(1 << 20);
  let marks = std::env::var("ROLLDOWN_MEM_BIG_MARKS")
    .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
    .unwrap_or_default();
  let mut info = DlInfo {
    dli_fname: std::ptr::null(),
    dli_fbase: std::ptr::null_mut(),
    dli_sname: std::ptr::null(),
    dli_saddr: std::ptr::null_mut(),
  };
  // SAFETY: `init` is a function of this library; `info` is writable.
  if unsafe { dladdr(init as *const c_void, &raw mut info) } != 0 {
    BASE.store(info.dli_fbase as usize, Ordering::Relaxed);
  }
  // The first call of `backtrace` loads libgcc_s; do it before tracking starts.
  let mut frames = [std::ptr::null_mut::<c_void>(); 4];
  // SAFETY: the buffer has 4 entries.
  unsafe { backtrace(frames.as_mut_ptr(), 4) };
  let _ = PATH.set(path);
  let _ = MARKS.set(marks);
  ENABLED.store(true, Ordering::Relaxed);
  MIN.store(min, Ordering::Relaxed);
}

/// Called at every mark of `rolldown_utils::mem_probe`.
pub fn on_mark(name: &str) {
  if MARKS.get().is_some_and(|m| m.iter().any(|p| name.starts_with(p.as_str()))) {
    dump(name);
  }
}

/// Appends the live big allocations, grouped by call stack, with the tag `tag`.
pub fn dump(tag: &str) {
  if !ENABLED.load(Ordering::Relaxed) {
    return;
  }
  guarded(|| {
    let snapshot: Vec<(usize, [usize; FRAMES])> = match TABLE.lock() {
      Ok(t) => t.as_ref().map(|t| t.values().copied().collect()).unwrap_or_default(),
      Err(_) => return,
    };
    let mut groups: HashMap<[usize; FRAMES], (usize, usize)> = HashMap::default();
    for (size, ips) in snapshot {
      let g = groups.entry(ips).or_default();
      g.0 += size;
      g.1 += 1;
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by(|a, b| b.1.0.cmp(&a.1.0));
    let base = BASE.load(Ordering::Relaxed);
    let t = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map_or(0, |d| d.as_millis());
    let total: usize = groups.iter().map(|g| g.1.0).sum();
    let mut out = format!("# t={t} tag={tag} total={total} groups={}\n", groups.len());
    for (ips, (bytes, count)) in groups.iter().take(200) {
      out.push_str(&format!("{tag} {bytes} {count}"));
      for &ip in ips.iter().filter(|&&ip| ip != 0) {
        // `addr2line` wants the address of the call, so step back one byte from the return address.
        if base != 0 && ip > base && ip - base < (1 << 30) {
          out.push_str(&format!(" b+{:#x}", ip - base - 1));
        } else {
          out.push_str(&format!(" {ip:#x}"));
        }
      }
      out.push('\n');
    }
    if let Some(path) = PATH.get()
      && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path)
    {
      let _ = f.write_all(out.as_bytes());
    }
  });
}
