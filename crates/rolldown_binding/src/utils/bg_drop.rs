//! perf/bun-gap N3 (PoC, `RD_BG_FINALIZER_BG=1`): free what a napi finalizer would drop on a background
//! thread, so the JS thread does not pay for it at exit (the last `PluginDriver`, the outputs' modules).

use std::sync::{OnceLock, mpsc};

pub fn enabled() -> bool {
  rolldown_utils::bg_flag!("RD_BG_FINALIZER_BG")
}

/// Drop `value` on the background dropper thread.
pub fn bg_drop<T: Send + 'static>(value: T) {
  static TX: OnceLock<mpsc::Sender<Box<dyn Send>>> = OnceLock::new();
  let tx = TX.get_or_init(|| {
    let (tx, rx) = mpsc::channel::<Box<dyn Send>>();
    std::thread::Builder::new()
      .name("rolldown-bg-drop".into())
      .spawn(move || rx.into_iter().for_each(drop))
      .expect("spawn the dropper thread");
    tx
  });
  let _ = tx.send(Box::new(value));
}
