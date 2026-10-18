//! perf/bun-gap A3 (PoC, `RD_BG_PAR_CODEGEN=1`): print a minified program in parts, in parallel.
//!
//! The top-level statements are cut into contiguous groups of similar source size. Each group
//! becomes its own `Program` (same source text, the comments attached inside its range) and is
//! printed by its own `Codegen` on a rayon thread, reading the mangled names from the one shared
//! `Scoping`. The pieces are concatenated; their sourcemaps are merged by shifting generated
//! positions and renumbering `names` in first-seen order, which is what a single `Codegen` emits.

use std::borrow::Cow;

use oxc::{
  allocator::{Allocator, Vec as ArenaVec},
  ast::{
    ast::{Class, Program},
    builder::AstBuilder,
  },
  ast_visit::{Visit, walk},
  codegen::{Codegen, CodegenOptions, LineOffsetTables},
  semantic::Scoping,
  span::{GetSpan, Span},
  syntax::class::ClassId,
};
use oxc_index::IndexVec;
use oxc_sourcemap::{SourceMap, SourceMapParts, Token};
use oxc_str::CompactStr;
use rayon::prelude::*;
use rustc_hash::FxHashMap;

type PrivateMappings = IndexVec<ClassId, FxHashMap<String, CompactStr>>;

struct SyncPtr<T>(*const T);
// SAFETY: the pointees are only read while the parallel section runs, and each part `Program`
// is printed by exactly one thread.
unsafe impl<T> Send for SyncPtr<T> {}
unsafe impl<T> Sync for SyncPtr<T> {}

pub fn par_codegen<'a>(
  allocator: &'a Allocator,
  program: &mut Program<'a>,
  options: &CodegenOptions,
  scoping: Option<&Scoping>,
  private: Option<&PrivateMappings>,
  line_tables: Option<std::sync::Arc<LineOffsetTables>>,
) -> Option<(String, Option<SourceMap<'a>>)> {
  let n = program.body.len();
  let mut k = rayon::current_num_threads().min(16);
  // RD_BG_PAR_MIN_STMTS: lower the split threshold so small test fixtures exercise the split.
  let min_stmts = std::env::var("RD_BG_PAR_MIN_STMTS").ok().and_then(|v| v.parse::<usize>().ok());
  if let Some(min) = min_stmts {
    if n < min {
      return None;
    }
    k = k.min(n).max(2);
  } else if k < 2 || n < 4 * k {
    return None;
  }
  if n < 2 || !options.minify {
    // Only the whitespace-free output is split: indentation and newlines carry state across
    // statements otherwise.
    return None;
  }
  // A part must not start right after a comment: codegen decides the separator before a comment
  // from what it printed last. Allowed cut before statement `i` only if no comment is attached
  // between the end of `i - 1` and the start of `i`.
  let comments_printed = options.comments != oxc::codegen::CommentOptions::disabled();
  let comment_positions: Vec<u32> = if comments_printed {
    program.comments.iter().map(|c| c.attached_to).collect()
  } else {
    Vec::new()
  };
  let can_cut = |i: usize| -> bool {
    if !comments_printed {
      return true;
    }
    let lo = program.body[i - 1].span().end;
    let hi = program.body[i].span().start;
    let idx = comment_positions.partition_point(|&p| p < lo);
    !(idx < comment_positions.len() && comment_positions[idx] <= hi)
  };
  // Cut points by source size of the statements.
  let weights: Vec<u64> =
    program.body.iter().map(|s| u64::from((s.span().size()).max(1))).collect();
  let total: u64 = weights.iter().sum();
  let mut cuts = vec![0usize];
  let mut acc = 0u64;
  for (i, w) in weights.iter().enumerate() {
    acc += w;
    let next = cuts.len() as u64;
    if i + 1 < n && acc * (k as u64) >= total * next && (cuts.len() as usize) < k && can_cut(i + 1) {
      cuts.push(i + 1);
    }
  }
  cuts.push(n);
  if cuts.len() < 3 {
    return None;
  }
  let starts: Vec<u32> = cuts.iter().map(|&c| if c < n { program.body[c].span().start } else { u32::MAX }).collect();

  let builder = AstBuilder::new(allocator);
  let body = std::mem::replace(&mut program.body, ArenaVec::new_in(&allocator));
  let mut stmts = body.into_iter();
  let groups = cuts.len() - 1;
  let mut parts: Vec<Program<'a>> = Vec::with_capacity(groups);
  for g in 0..groups {
    let len = cuts[g + 1] - cuts[g];
    let mut part_body = ArenaVec::with_capacity_in(len, &allocator);
    for _ in 0..len {
      part_body.push(stmts.next().expect("statement"));
    }
    let lo = if g == 0 { 0 } else { starts[g] };
    let hi = if g + 1 == groups { u32::MAX } else { starts[g + 1] };
    let comments = ArenaVec::from_iter_in(
      program.comments.iter().filter(|c| c.attached_to >= lo && c.attached_to < hi).cloned(),
      &allocator,
    );
    let span = if g + 1 == groups { program.span } else { Span::new(program.span.start, hi) };
    let directives = if g == 0 {
      std::mem::replace(&mut program.directives, ArenaVec::new_in(&allocator))
    } else {
      ArenaVec::new_in(&allocator)
    };
    parts.push(Program::new(
      span,
      program.source_type,
      program.source_text,
      comments,
      if g == 0 { program.hashbang.take() } else { None },
      directives,
      part_body,
      &builder,
    ));
  }

  let t_split = std::time::Instant::now();
  let scoping_ptr = scoping.map(|s| SyncPtr(std::ptr::from_ref(s)));
  let part_ptrs: Vec<SyncPtr<Program<'a>>> = parts.iter().map(|p| SyncPtr(std::ptr::from_ref(p))).collect();
  // Codegen numbers classes in print order (private member mappings are indexed by it), so a
  // part starts after the classes of the parts before it.
  let class_counts: Vec<usize> = part_ptrs
    .par_iter()
    .map(|p| {
      // SAFETY: see `SyncPtr`.
      let part: &Program<'a> = unsafe { &*p.0 };
      let mut counter = ClassCounter(0);
      counter.visit_program(part);
      counter.0
    })
    .collect();
  let mut class_offsets = Vec::with_capacity(groups);
  let mut acc = 0;
  for c in &class_counts {
    class_offsets.push(acc);
    acc += c;
  }
  let outputs: Vec<(String, Option<SourceMap<'a>>)> = part_ptrs
    .par_iter()
    .zip(class_offsets.par_iter())
    .map(|(p, &class_offset)| {
      // SAFETY: see `SyncPtr`.
      let part: &Program<'a> = unsafe { &*p.0 };
      let mut cg = Codegen::new()
        .with_options(options.clone())
        .with_private_member_mappings(private.cloned())
        .with_class_id_offset(class_offset);
      if let Some(tables) = &line_tables {
        cg = cg.with_sourcemap_line_tables(std::sync::Arc::clone(tables));
      }
      if let Some(s) = &scoping_ptr {
        // SAFETY: the scoping outlives this call and is not mutated during it.
        cg = unsafe { cg.with_scoping_ptr(&*s.0) };
      }
      let t = std::time::Instant::now();
      let ret = cg.build(part);
      if std::env::var_os("RD_BG_TIMING").is_some() {
        #[expect(clippy::print_stderr, reason = "perf investigation output")]
        {
          eprintln!("[bg] par_codegen part stmts={} ms={:.3} out={}", part.body.len(), t.elapsed().as_secs_f64() * 1000.0, ret.code.len());
        }
      }
      (ret.code, ret.map)
    })
    .collect();
  let t_par = std::time::Instant::now();

  // Join. Generated-position offsets of each part come from the text before it; `names` are
  // renumbered in first-seen order (serial, small); tokens are translated in parallel.
  let mut code = String::with_capacity(outputs.iter().map(|(c, _)| c.len()).sum());
  let mut offsets = Vec::with_capacity(outputs.len());
  let (mut line_off, mut col_off) = (0u32, 0u32);
  for (part_code, _) in &outputs {
    offsets.push((line_off, col_off));
    match memchr_rev_newline(part_code) {
      Some(pos) => {
        line_off += part_code.bytes().filter(|&b| b == b'\n').count() as u32;
        col_off = utf16_len(&part_code[pos + 1..]);
      }
      None => col_off += utf16_len(part_code),
    }
    code.push_str(part_code);
  }
  let mut names: Vec<Cow<'a, str>> = Vec::new();
  let mut name_ids: FxHashMap<Cow<'a, str>, u32> = FxHashMap::default();
  let mut sources = Vec::new();
  let mut source_contents = Vec::new();
  let mut has_map = false;
  let mut part_tokens: Vec<(Box<[Token]>, Vec<u32>)> = Vec::with_capacity(outputs.len());
  for (_, part_map) in outputs {
    let Some(map) = part_map else {
      part_tokens.push((Box::new([]), Vec::new()));
      continue;
    };
    has_map = true;
    let SourceMapParts { names: part_names, sources: s, source_contents: sc, tokens, .. } =
      map.into_parts();
    if sources.is_empty() {
      sources = s;
      source_contents = sc;
    }
    let remap: Vec<u32> = part_names
      .into_iter()
      .map(|name| {
        *name_ids.entry(name.clone()).or_insert_with(|| {
          names.push(name);
          (names.len() - 1) as u32
        })
      })
      .collect();
    part_tokens.push((tokens, remap));
  }
  // A single codegen skips a mapping whose original position equals the previous one.
  let mut skip_first = vec![false; part_tokens.len()];
  let mut last_src = None;
  for (i, (tokens, _)) in part_tokens.iter().enumerate() {
    if let Some(first) = tokens.first() {
      let src = (first.get_src_line(), first.get_src_col(), first.get_source_id());
      skip_first[i] = last_src == Some(src);
      let last = tokens.last().expect("non-empty");
      last_src = Some((last.get_src_line(), last.get_src_col(), last.get_source_id()));
    }
  }
  let total: usize =
    part_tokens.iter().zip(&skip_first).map(|((t, _), &sk)| t.len() - usize::from(sk && !t.is_empty())).sum();
  let mut tokens: Vec<Token> = Vec::with_capacity(total);
  {
    let mut rest = &mut tokens.spare_capacity_mut()[..total];
    let mut jobs = Vec::with_capacity(part_tokens.len());
    for (i, (t, remap)) in part_tokens.iter().enumerate() {
      let src = if skip_first[i] && !t.is_empty() { &t[1..] } else { &t[..] };
      let (head, tail) = rest.split_at_mut(src.len());
      rest = tail;
      jobs.push((head, src, remap, offsets[i]));
    }
    jobs.into_par_iter().for_each(|(dst, src, remap, (lo, co))| {
      for (d, t) in dst.iter_mut().zip(src) {
        let dl = t.get_dst_line();
        d.write(Token::new(
          dl + lo,
          if dl == 0 { t.get_dst_col() + co } else { t.get_dst_col() },
          t.get_src_line(),
          t.get_src_col(),
          t.get_source_id(),
          t.get_name_id().map(|id| remap[id as usize]),
        ));
      }
    });
  }
  // SAFETY: the jobs above wrote all `total` elements.
  unsafe { tokens.set_len(total) };
  let map = has_map.then(|| {
    SourceMap::from_parts(SourceMapParts {
      file: None,
      names,
      source_root: None,
      sources,
      source_contents,
      tokens: tokens.into_boxed_slice(),
      token_chunks: None,
      ignore_list: None,
      debug_id: None,
    })
  });
  if std::env::var_os("RD_BG_TIMING").is_some() {
    #[expect(clippy::print_stderr, reason = "perf investigation output")]
    {
      eprintln!(
        "[bg] par_codegen groups={groups} par={:.3} join={:.3}",
        (t_par - t_split).as_secs_f64() * 1000.0,
        t_par.elapsed().as_secs_f64() * 1000.0
      );
    }
  }
  Some((code, map))
}

struct ClassCounter(usize);

impl<'a> Visit<'a> for ClassCounter {
  fn visit_class(&mut self, class: &Class<'a>) {
    self.0 += 1;
    walk::walk_class(self, class);
  }
}

fn memchr_rev_newline(s: &str) -> Option<usize> {
  s.as_bytes().iter().rposition(|&b| b == b'\n')
}

fn utf16_len(s: &str) -> u32 {
  if s.is_ascii() { s.len() as u32 } else { s.chars().map(char::len_utf16).sum::<usize>() as u32 }
}
