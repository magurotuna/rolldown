//! Usage: bg_bench <options.json> [runs]
//! cwd = workload dir. `options.json` is a `BundlerOptions` JSON (camelCase, as in `_config.json`),
//! plus the harness-only key `externalRegex: [String]`.
//! Prints per-run ms of `BundleFactory::new` + `create_bundle` + `write()` (same path as the JS API).
#![expect(clippy::print_stdout, clippy::print_stderr)]

#[global_allocator]
static ALLOC: mimalloc_safe::MiMalloc = mimalloc_safe::MiMalloc;

use std::time::Instant;

use rolldown::{BundleFactory, BundleFactoryOptions, BundlerOptions};
use rolldown_common::{BundleMode, IsExternal, SourceMapIgnoreList};
use rolldown_utils::{js_regex::HybridRegex, pattern_filter::StringOrRegex};

fn main() {
  let mut args = std::env::args().skip(1);
  let path = args.next().expect("options.json");
  let runs: usize = args.next().map_or(1, |s| s.parse().unwrap());
  let mut value: serde_json::Value =
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
  let external_regex = value.as_object_mut().unwrap().remove("externalRegex");
  // Harness-only `bgMinify: { compress, mangle, removeWhitespace }`: the object form of `minify`,
  // which the Rust options JSON cannot express. `compress` is a bool or `{ sequences, joinVars }`.
  let bg_minify = value.as_object_mut().unwrap().remove("bgMinify");
  let mut options: BundlerOptions = serde_json::from_value(value).unwrap();
  if let Some(m) = bg_minify {
    let flag = |key: &str| m.get(key).and_then(serde_json::Value::as_bool).unwrap_or(true);
    let compress = match m.get("compress") {
      Some(serde_json::Value::Object(c)) => Some(rolldown_common::RawCompressOptions {
        sequences: c.get("sequences").and_then(serde_json::Value::as_bool),
        join_vars: c.get("joinVars").and_then(serde_json::Value::as_bool),
        ..Default::default()
      }),
      _ => flag("compress").then(rolldown_common::RawCompressOptions::default),
    };
    options.minify = Some(rolldown_common::RawMinifyOptions::Object(
      rolldown_common::RawMinifyOptionsDetailed {
        mangle: flag("mangle").then(rolldown_common::RawMangleOptions::default),
        mangle_properties: None,
        compress,
        remove_whitespace: flag("removeWhitespace"),
        ascii_only: false,
      },
    ));
  }
  if let Some(serde_json::Value::Array(res)) = external_regex {
    options.external = Some(IsExternal::StringOrRegex(
      res
        .into_iter()
        .map(|r| StringOrRegex::Regex(HybridRegex::new(r.as_str().unwrap()).unwrap()))
        .collect(),
    ));
  }
  // Same default as the JS API (`sourcemapIgnoreList ?? /node_modules/`).
  if options.sourcemap_ignore_list.is_none() {
    options.sourcemap_ignore_list = Some(SourceMapIgnoreList::from_string_or_regex(
      StringOrRegex::Regex(HybridRegex::new("node_modules").unwrap()),
    ));
  }
  if options.cwd.is_none() {
    options.cwd = Some(std::env::current_dir().unwrap());
  }

  let max_blocking_threads = std::env::var("ROLLDOWN_MAX_BLOCKING_THREADS")
    .ok()
    .and_then(|v| v.parse::<usize>().ok())
    .unwrap_or(4);
  let worker_threads = std::env::var("ROLLDOWN_WORKER_THREADS")
    .ok()
    .and_then(|v| v.parse::<usize>().ok())
    .unwrap_or(num_cpus::get_physical() * 3 / 2);
  let rt = tokio::runtime::Builder::new_multi_thread()
    .max_blocking_threads(max_blocking_threads)
    .worker_threads(worker_threads)
    .thread_name("rolldown-worker")
    .enable_all()
    .build()
    .unwrap();

  let mut times = Vec::new();
  for _ in 0..runs {
    let options = options.clone();
    let t0 = Instant::now();
    // Spawn onto a worker like napi's `execute_future` does (block_on would run it on the main
    // thread, where spawns go through tokio's global inject queue).
    let warnings = rt.block_on(rt.spawn(async move {
      let mut factory = BundleFactory::new(BundleFactoryOptions {
        bundler_options: options,
        plugins: vec![],
        session: None,
        disable_tracing_setup: false,
      })
      .unwrap();
      let bundle = factory.create_bundle(BundleMode::FullBuild, None).unwrap();
      match bundle.write().await {
        Ok(out) => out.warnings.len(),
        Err(errs) => {
          for e in errs.iter() {
            eprintln!("{}", e.to_diagnostic());
          }
          std::process::exit(1);
        }
      }
    }))
    .unwrap();
    times.push((t0.elapsed().as_secs_f64() * 1000.0, warnings));
  }
  let rec = format!(
    "{{\"times\":[{}],\"warnings\":{}}}",
    times.iter().map(|(t, _)| format!("{t:.3}")).collect::<Vec<_>>().join(","),
    times[0].1
  );
  if let Ok(log) = std::env::var("API_LOG") {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
    writeln!(f, "{rec}").unwrap();
  }
  println!("{rec}");
}
