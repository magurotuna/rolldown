# Global allocator — Design & Principles

## Summary

The binding uses mimalloc as the global allocator by default. The `jemalloc` cargo feature of `rolldown_binding` replaces it with jemalloc (`tikv-jemallocator`), with transparent huge pages (THP) off. The feature is opt-in and Linux only. It exists for large builds that run out of memory: it lowers the peak RSS, and the cost in build time depends on the project and the machine.

## Design principles / Goals

1. **mimalloc stays the default.** The effect of jemalloc on build time depends on the project and the machine. In the measurements below, it ranges from 6% faster to 9% slower.
2. **jemalloc runs with THP off.** On Linux with THP set to `always`, the kernel maps a whole 2 MiB huge page on the first touch. With THP on, jemalloc returns only parts of huge pages. RSS then drops, but the memory stays charged to the cgroup until the kernel splits the huge page (deferred split). With `thp:never`, the memory that jemalloc purges leaves the process and the cgroup.
3. **The options are compiled in.** `malloc_conf` in `crates/rolldown_binding/src/lib.rs` sets `thp:never,dirty_decay_ms:1000,muzzy_decay_ms:0,background_thread:true`. `_RJEM_MALLOC_CONF` overrides them at run time.
4. **The feature excludes the other allocator features.** `default_global_allocator` and `tracking_allocator` also choose the global allocator, so a build with two of them stops with a `compile_error!`.

## Measurements

Linux VM (16 vCPU, 64 GiB, AMD EPYC, kernel 6.12, THP `always`). The values are medians of 5 paired rounds.

This feature, against mimalloc, with #11774 and #11801:

| Project | Peak RSS (mimalloc) | Peak RSS change with the feature | Wall time change | CPU time change |
|---|---:|---:|---:|---:|
| Large React Router app (client and SSR builds, `build.sourcemap: true`, minify) | 16.9 GiB | −4.16 GiB | +2.8% | +2.2% to +4.4% |
| [sourcemap-chain-bench](https://github.com/magurotuna/sourcemap-chain-bench) | 7.2 GiB | −0.73 GiB | +3.6% (−5.9% to +6.1%) | −1.0% to 0% |

In the same rounds, the preloaded jemalloc (see below) lowered the peak more: −5.48 GiB and −0.82 GiB. With the feature, only the allocations of the binding use jemalloc. With the preloaded jemalloc, Node also uses jemalloc.

These measurements preloaded the system jemalloc 5.3.0 (`LD_PRELOAD`, so Node also used jemalloc) into a binding built with `default_global_allocator`, with the options above:

| Project | Peak RSS (mimalloc) | Peak RSS change with jemalloc | Build time change |
|---|---:|---:|---:|
| Large React Router app (client and SSR builds, `build.sourcemap: true`, minify), with #11801 and #11774 | 16.1 GiB | −4.7 GiB | −2% to −6% (wall), 0% to −3% (CPU) |
| [sourcemap-chain-bench](https://github.com/magurotuna/sourcemap-chain-bench), with #11801 and #11774 | 7.1 GiB | −0.85 GiB | +1% to +9% (wide range) |
| Large React Router app, on an earlier `main` and an older CPU | 23.4 GiB | −7.4 GiB | +2.7% to +6.3% |

The output did not change.

## Rejected alternatives

- **Turn THP off for mimalloc's arenas.** This lowered the peak by 1.5 to 3 GiB, but builds were 5% to 8% slower. Without huge pages, the worker threads fault in more pages, and the main thread waits for the mmap lock.
- **jemalloc with THP on (its default).** RSS dropped, but the cgroup peak did not (deferred split, see principle 2).
- **glibc malloc with tuned arenas.** This lowered the peak by about 7 GiB, but builds were 26% to 34% slower.

## Unresolved Questions

- Why jemalloc returns more of the memory freed during a build than mimalloc does. One explanation is fragmentation: in the large app, mimalloc's live memory is about 6 GiB after the client build, while RSS is about 17 GiB. This explanation has not been confirmed at the page level.
- Whether the npm package should ship a jemalloc binding. Today a user has to build the binding with this feature.
