# Global allocator — Design & Principles

## Summary

The binding uses mimalloc as the global allocator by default. The `jemalloc` cargo feature of `rolldown_binding` replaces it with jemalloc (`tikv-jemallocator`), with transparent huge pages (THP) off. The feature is opt-in and Linux only. It exists for large builds that run out of memory: it lowers the peak RSS, and the cost in build time depends on the project and the machine.

## Design principles / Goals

1. **mimalloc stays the default.** The effect of jemalloc on build time depends on the project and the machine. In the measurements below, the median wall time with the feature is 2.8% to 3.6% higher.
2. **jemalloc runs with THP off.** On Linux with THP set to `always`, the kernel maps a whole 2 MiB huge page on the first touch. With THP on, jemalloc returns only parts of huge pages. RSS then drops, but the memory stays charged to the cgroup until the kernel splits the huge page (deferred split). With `thp:never`, the memory that jemalloc purges leaves the process and the cgroup.
3. **The options are compiled in.** `MALLOC_CONF` in `crates/rolldown_binding/src/lib.rs` (exported as `_rjem_malloc_conf`) sets `thp:never,dirty_decay_ms:1000,muzzy_decay_ms:0,background_thread:true`. `_RJEM_MALLOC_CONF` overrides them at run time.
4. **The other allocator features take precedence.** `default_global_allocator` and `tracking_allocator` also choose the global allocator. The binding compiles in one global allocator, in this order: `default_global_allocator`, `tracking_allocator`, `jemalloc`, mimalloc. So `cargo check --all-features` (`just lint-rust`) builds with the system allocator, as it does without this feature. On a platform other than Linux, the `jemalloc` feature has no effect.

## Measurements

Linux VM (16 vCPU, 64 GiB, AMD EPYC, kernel 6.12, THP `always`). Each value is the median of the per-round differences in paired rounds with a rotating order. The range over the rounds is in parentheses. "Mean baseline" compares with the mean of the two baseline runs in the same round.

This feature, against mimalloc, 5 rounds. Both builds include #11801, #11774, and an unmerged change that frees the render chunk maps before minification:

| Project | Peak RSS (mimalloc) | Peak RSS change | Wall time change | CPU time change (first baseline) | CPU time change (mean baseline) |
|---|---:|---:|---:|---:|---:|
| Large React Router app (client and SSR builds, `build.sourcemap: true`, minify) | 16.9 GiB | −4.16 GiB (−4.36 to −4.14) | +2.8% (−0.1 to +4.9) | +4.4% (−2.3 to +5.7) | +2.2% (−1.6 to +4.9) |
| [sourcemap-chain-bench](https://github.com/magurotuna/sourcemap-chain-bench) | 7.2 GiB | −0.73 GiB (−0.85 to −0.51) | +3.6% (−5.9 to +6.1) | 0.0% (−7.9 to +3.5) | −1.0% (−3.7 to +2.2) |

On sourcemap-chain-bench, two baseline runs of the same build differed by +3.4% in wall time, so the wall time change there is within the noise.

In the same rounds, the system jemalloc 5.3.0, preloaded with `LD_PRELOAD` into a binding built with `default_global_allocator` (so Node also used jemalloc), lowered the peak more: −5.48 GiB (wall time −2.6%) and −0.82 GiB. With this feature, only the allocations of the binding use jemalloc.

The preloaded jemalloc on `main` without other changes, on the large app, 3 rounds: −8.06 GiB (−8.15 to −7.46) from 22.95 GiB, wall time −0.9% (−1.3 to +0.9), CPU time +2.3% (+1.1 to +5.1). On an earlier `main` and an older CPU, it was −7.4 GiB with wall time +2.7% (−0.0 to +3.1).

The output (`dist/` with `.map` files) matched in every run, after the hashes in file names and contents were replaced with zeros. One generated SVG file, whose content order changes from run to run, was left out of the comparison.

## Rejected alternatives

- **Turn THP off for mimalloc's arenas.** This lowered the peak by 1.5 to 3 GiB, but builds were 5% to 8% slower. Without huge pages, the worker threads fault in more pages, and the main thread waits for the mmap lock.
- **jemalloc with THP on (its default).** RSS dropped, but the cgroup peak did not (deferred split, see principle 2).
- **glibc malloc with tuned arenas.** This lowered the peak by about 7 GiB, but builds were 26% to 34% slower.

## Unresolved Questions

- Why jemalloc returns more of the memory freed during a build than mimalloc does. One explanation is fragmentation: in the large app, mimalloc's live memory is about 6 GiB after the client build, while RSS is about 17 GiB. This explanation has not been confirmed at the page level.
- Whether the npm package should ship a jemalloc binding. Today a user has to build the binding with this feature.
