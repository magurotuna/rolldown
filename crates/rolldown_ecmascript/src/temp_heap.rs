//! Experiment: minify the largest chunk of a build in a temporary heap.
//!
//! The environment variable `ROLLDOWN_LARGEST_MINIFY` selects how the largest chunk is minified:
//!
//! - unset or `pool`: like every other chunk, with an arena from the minifier's pool.
//! - `fresh-arena`: with a new arena from the global allocator. The arena is freed at the end,
//!   instead of going back to the pool.
//! - `heap-arena`: with a new arena whose chunks come from a temporary heap. Only the arena is in
//!   the temporary heap.
//! - `heap`: the calling thread allocates everything from a temporary heap while the closure runs.
//!
//! An embedder (for example `rolldown_binding`) provides the temporary heap with
//! [`set_temp_heap_runner`]. Without a runner, both heap modes fall back to `fresh-arena`.

use std::{alloc::GlobalAlloc, sync::OnceLock};

use oxc::allocator::Allocator;

/// Runs the closure with a new heap, then deletes the heap. Live blocks in the heap stay valid.
///
/// If `whole_thread` is true, the calling thread allocates from the heap while the closure runs,
/// and the closure gets `None`. Otherwise the closure gets an allocator for the heap, or `None` if
/// no heap could be created.
pub type TempHeapRunner =
  fn(whole_thread: bool, f: &mut dyn FnMut(Option<&'static (dyn GlobalAlloc + Sync)>));

static TEMP_HEAP_RUNNER: OnceLock<TempHeapRunner> = OnceLock::new();

/// Register the runner for the temporary heap. Only the first call has an effect.
pub fn set_temp_heap_runner(runner: TempHeapRunner) {
  let _ = TEMP_HEAP_RUNNER.set(runner);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LargestMinifyMode {
  Pool,
  FreshArena,
  HeapArena,
  Heap,
}

fn largest_minify_mode() -> LargestMinifyMode {
  static MODE: OnceLock<LargestMinifyMode> = OnceLock::new();
  *MODE.get_or_init(|| match std::env::var("ROLLDOWN_LARGEST_MINIFY").as_deref() {
    Ok("fresh-arena") => LargestMinifyMode::FreshArena,
    Ok("heap-arena") => LargestMinifyMode::HeapArena,
    Ok("heap") => LargestMinifyMode::Heap,
    _ => LargestMinifyMode::Pool,
  })
}

/// Runs `f` for the largest chunk of a build, with the arena that `ROLLDOWN_LARGEST_MINIFY`
/// selects. `f` gets `None` when it should use the minifier's pool.
pub fn with_largest_minify_allocator<R>(f: impl FnOnce(Option<&Allocator>) -> R) -> R {
  let mode = largest_minify_mode();
  let runner = TEMP_HEAP_RUNNER.get();
  let whole_thread = match (mode, runner) {
    (LargestMinifyMode::Pool, _) => return f(None),
    (LargestMinifyMode::FreshArena, _) | (_, None) => return f(Some(&Allocator::default())),
    (LargestMinifyMode::HeapArena, Some(_)) => false,
    (LargestMinifyMode::Heap, Some(_)) => true,
  };
  let runner = runner.expect("checked above");
  let mut f = Some(f);
  let mut result = None;
  runner(whole_thread, &mut |chunk_alloc| {
    // The arena is dropped before the runner deletes the heap.
    let allocator = match chunk_alloc {
      Some(chunk_alloc) => Allocator::with_capacity_in(0, chunk_alloc),
      None => Allocator::default(),
    };
    let f = f.take().expect("the runner calls the closure once");
    result = Some(f(Some(&allocator)));
  });
  result.expect("the runner calls the closure once")
}
