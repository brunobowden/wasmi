# Linear memory and the platform allocator: findings

Working notes for the lazy-growth change. They go into that PR's description (and possibly a doc
comment in `ByteBuffer`) before it is submitted; this file is not meant to be merged.

Question: how many physical pages does a linear memory commit, on creation and on growth, under
each platform's system allocator? `phys_footprint` (`task_info(TASK_VM_INFO)`) is the figure iOS
enforces; on Linux, resident memory (`VmRSS`).

## Summary

| | Linux (glibc) | macOS 27 (M4 Max) | iOS 27.2 (iPhone 18 Pro Max) |
| --- | --- | --- | --- |
| Freed large blocks returned to the OS | Yes, immediately (`munmap`) | **No**: libmalloc's large cache keeps them in the footprint | **Mostly no**: 109 of 128 MiB still counted 1 s after `free`; some released later |
| `alloc_zeroed` of a fresh block | Lazy | Lazy | Lazy only when not served from the cache |
| `alloc_zeroed` served from cached (dirty) blocks | n/a | Resident: `calloc` clears it (0.3–0.5 ms per 64 MiB vs 0.0 ms fresh) | Resident (inferred: a 256 MiB `calloc` took 16–22 ms) |
| Growth via `realloc` + zero-fill (wasmi 2.0, #2064) | Commits every grown page | Commits every grown page | Commits every grown page |
| Growth via "doubled zeroed block, copy non-zero chunks, free old" | Lazy | **Worse than `realloc`**: 2× footprint when written, no saving when untouched | Not measurable yet (see below); slowest single grow 25–36 ms vs 1–6 ms |

Conclusion: lazy growth must not depend on freeing blocks back to `malloc` and allocating new
ones. Reserving zeroed capacity up front (bounded by the memory's maximum or a configured
reservation), growing within it by bumping `len`, and falling back to `realloc` + zero-fill beyond
it behaves the same under every allocator. The cache also means `alloc_zeroed` alone (#2064) is
only lazy for blocks the allocator hasn't recycled: an embedder that creates and drops instances
repeatedly can get recycled blocks that `calloc` has to clear.

## macOS (libmalloc)

Measured 10/27/2026 on an M4 Max, macOS 27, by growing one buffer 1 page at a time to 256 MiB.

| Pattern | Untouched | Every page written | Slowest single grow |
| --- | --: | --: | --: |
| `realloc` + zero-fill (v2.0.0) | 259 MiB | 259 MiB | 0.2–0.3 ms |
| Doubled `calloc`, copy non-zero 4 KiB chunks, `free` old | 257 MiB | 513 MiB | 12–14 ms |
| Same, launched with `MallocLargeCache=0` | **0 MiB** | **257 MiB** | |
| Growth in one large step | 62 KiB | | |

- Reproduced in plain C (`calloc`/`memcmp`/`memcpy`/`free`, one process per case, `-O0`), so
  it is the allocator, not wasmi. A written-then-freed 128 MiB `malloc` block still counted
  129 MiB after `free`.
- Cause: libmalloc keeps recently freed large blocks resident for reuse (the large "death row"
  cache; `vmmap` shows dirty `Malloc Large (empty)`). With ≥ 32 GiB of RAM it holds up to 64
  blocks of up to 512 MiB; below that, 16 blocks of up to 128 MiB (PhotoSort PR #121).
  Disabling it with `MallocLargeCache=0` made the copy approach behave as on Linux: untouched
  growth fully lazy, written growth exactly the final size, everything returned after the last
  `free`.
- `malloc_zone_pressure_relief` does not release these blocks (rawtonic PR #381).
- At 1 GiB: 2,049 MiB with the copy approach vs 1,026 MiB with `realloc`; slowest grow 52–99 ms
  vs under 1.5 ms.

Full matrix (10/27/2026, M4 Max, 128 GiB, macOS 26.6.2; every case in a fresh process, each with
and without `MallocLargeCache=0`; the Rust runner and plain C agree to within 1 MiB):

| Case, grown 1 page at a time | 256 MiB | 256 MiB, cache off | 1 GiB | 1 GiB, cache off | Slowest grow |
| --- | --: | --: | --: | --: | --: |
| `realloc` + zero-fill, untouched | 258 | 256 | 1,027 | 1,025 | ≤ 0.8 ms |
| `realloc` + zero-fill, written | 258 | 256 | 1,027 | 1,025 | ≤ 0.2 ms |
| Copy-and-free, untouched | 256 | **0** | 1,025 | **0** | 12–34 ms |
| Copy-and-free, written | **512** | 256 | **2,049** | 1,025 | 13–47 ms |
| Single `calloc`, untouched | 0 | 0 | 0 | 0 | |
| wasmi 2.0 `memory.grow`, untouched | 258 | 256 | 1,027 | 1,025 | ≤ 0.3 ms |

(MiB of `phys_footprint` after growth. With the cache on, freeing the buffer changed nothing; with
it off, everything returned to 0.)

- **Retention is indefinite on this Mac:** a freed 128 MiB block still counted 128 MiB after 1, 5
  and 15 s.
- **No cache limit reached:** 16 freed blocks of every size from 16 to 512 MiB (up to 8 GiB) all
  stayed counted, consistent with the reported 64 × ≤ 512 MiB limit on ≥ 32 GiB Macs.
- **Recycled blocks are cleared:** re-creating a 64 MiB `calloc` block took 0.3–0.5 ms per round
  with the cache vs 0.0 ms without. The footprint didn't grow only because the recycled block was
  already counted; from the second creation on, an untouched block costs its full size. With wasmi
  2.0.0 (no #2064), `Memory::new` committed 64 MiB per creation with the cache off.

## iOS (libmalloc, iPhone 18 Pro Max, iOS 27.2)

MyBench build 1019 (commit 7a99fc9b), 10/27/2026, Memory group. All cases ran in one app
process (iOS can't fork), in this order, each 1 page at a time to 256 MiB (the 1 GiB cases were
skipped by the memory headroom guard):

| Order | Case | Footprint before | During the case | After `free` | 1 s after `free` | Slowest grow |
| --: | --- | --: | --: | --: | --: | --: |
| 1 | Free retention: 128 MiB written, freed | 489 MiB | +128 MiB | **+109 MiB** | +109 MiB | |
| 2 | `realloc` + zero-fill, untouched | 598 MiB | +134 MiB | +134 MiB | +134 MiB | 5.6 ms |
| 3 | `realloc` + zero-fill, written | 731 MiB | +128 MiB | +128 MiB | **−159 MiB** | 1.0 ms |
| 4 | Copy-and-free, untouched | 572 MiB | −20 MiB | −20 MiB | −20 MiB | 35.6 ms |
| 5 | Copy-and-free, written | 553 MiB | −13 MiB | −13 MiB | −13 MiB | 25.2 ms |
| 6 | Single `calloc`, untouched | 540 MiB | −176 MiB | −176 MiB | −176 MiB | 22.4 ms |
| 7 | Single `calloc`, written | 364 MiB | −76 MiB | −76 MiB | −76 MiB | 16.2 ms |
| 8 | wasmi `memory.grow`, untouched | 288 MiB | **+192 MiB** | +192 MiB | +192 MiB | 1.1 ms |
| 9 | wasmi `memory.grow`, written | 480 MiB | −64 MiB | −64 MiB | −64 MiB | 0.3 ms |

Each case's starting footprint equals the previous case's ending footprint, so the cases form a
chain and each one inherits what the allocator retained before it.

What it shows:

1. **iOS retains freed large blocks too.** A written 128 MiB block still counted 109 MiB after
   `free`, and 1 s later. In every growth case, freeing the grown buffer did not lower the
   footprint at all ("after `free`" equals "during"). Same class of behaviour as the macOS large
   cache.
2. **The retained memory is released eventually, in bulk.** 1 s after case 3, the footprint fell
   by 287 MiB (from +128 to −159). Later cases started from progressively lower footprints and
   several measured negative growth: the cache was being reused or trimmed while they ran.
3. **wasmi 2.0's growth commits untouched pages on iOS.** Growing an untouched memory to 256 MiB
   added 192 MiB even with recycled blocks available (the other 64 MiB were most likely reused
   cached memory).
4. **Allocating from recycled blocks is slow, which suggests `calloc` clearing them.** A single
   256 MiB `calloc` took 16–22 ms. A fresh zero-filled mapping returns in microseconds, so these
   were most likely cached dirty blocks being zeroed, which also commits them. On macOS the same
   effect explains why untouched copy-and-free growth was not lazy (0 MiB with the cache off).
5. **The copy approach is slow on iOS:** slowest single grow 25–36 ms vs 0.3–5.6 ms for
   `realloc` and wasmi.
6. **Not yet measurable:** the footprint cost of each pattern by itself, because the cases share a
   process and the cache carries memory from one case to the next.

Second run (build 1019, 10/27/2026 11:03 PM; battery 20%, not a fresh launch, so timings are
noisy): same shape, different details. A freed 128 MiB block was mostly released within 1 s
(10 MiB left, vs 109 MiB in the first run). Grown to 256 MiB: `realloc` + zero-fill 237 MiB
untouched; copy-and-free 236 MiB untouched and 268 MiB written (no saving, but not the macOS 2×);
wasmi 2.0 220 MiB untouched. About 255 MiB was released between two cases. So iOS keeps freed
blocks like macOS but releases them sooner and unpredictably; per-case figures from a shared
process aren't reliable, and the copy approach saves nothing on iOS either.

## Linux (glibc)

- Freed `mmap`ed blocks are returned immediately, and reading never-written pages maps the shared
  zero page, so the copy approach is lazy: 256 MiB untouched, grown page by page, ended at 2 MiB
  resident (v2.0.0: 258 MiB).
- But it is slower when every page is written (worst single grow 72 ms vs 1.3 ms at 256 MiB,
  because glibc's `realloc` uses `mremap`), and with a raised `mmap` threshold freed heap blocks
  can stay resident (16 MiB fully written: 34 MiB vs 18 MiB).
- With transparent hugepages set to `always`, one write commits a whole 2 MiB page.

## Next measurements

- iOS, one case per app launch (or per fresh process on macOS), so cases can't inherit cached
  blocks: each growth pattern, untouched and written, at 256 MiB.
- iOS: create, drop and re-create memories of the same size (the MyBench `recycle` test, pending
  a device run). On macOS, recycled blocks are cleared and stay resident (above).
- Whether the iOS cache's size limits match macOS's below-32-GiB setting (16 blocks of up to
  128 MiB): the iPhone has 12 GB of RAM.
- How long iOS keeps retained blocks, and what triggers the release (time, pressure, size).

## Sources

- PhotoSort PR #121, "Return image-sized buffers to the kernel on macOS":
  https://github.com/duartebarbosadev/PhotoSort/pull/121
- rawtonic PR #381, `MallocLargeCache=0` and `malloc_zone_pressure_relief`:
  https://github.com/rawtonic-app/rawtonic/pull/381
- wasmtime malloc-backed linear memory:
  https://docs.wasmtime.dev/api/src/wasmtime/runtime/vm/memory/malloc.rs.html
