//! perf/bun-gap B1 (PoC, `RD_BG_ASSET_AT_FINALIZE=1`): resolve the asset module placeholders
//! (`module.exports = "__ROLLDOWN_ASSET__#<ref>"`, emitted by `AssetModulePlugin::load`) in the
//! module AST during `finalize_modules`, instead of rewriting the whole rendered chunk with a
//! hires `MagicString` in `renderChunk` (which also forces an extra `collapse_sourcemaps`).

use std::path::Path;

use oxc::{
  allocator::Allocator,
  ast::ast::StringLiteral,
  ast_visit::{VisitMut, walk_mut},
};
use oxc_str::Str;
use rolldown_common::FileEmitter;
use rolldown_std_utils::relative_path_as_js_specifier;

pub const PREFIX: &str = "__ROLLDOWN_ASSET__#";

pub fn enabled() -> bool {
  rolldown_utils::bg_flag!("RD_BG_ASSET_AT_FINALIZE")
}

pub struct AssetRewriter<'me, 'a> {
  pub alloc: &'a Allocator,
  pub file_emitter: &'me FileEmitter,
  pub chunk_filename: &'me str,
}

impl<'a> VisitMut<'a> for AssetRewriter<'_, 'a> {
  fn visit_string_literal(&mut self, lit: &mut StringLiteral<'a>) {
    if let Some(ref_id) = lit.value.as_str().strip_prefix(PREFIX)
      && let Ok(asset_filename) = self.file_emitter.get_file_name(ref_id)
    {
      let chunk_dir = Path::new(self.chunk_filename).parent().unwrap_or(Path::new(""));
      let relative = relative_path_as_js_specifier(asset_filename.as_str(), chunk_dir);
      lit.value = Str::from_str_in(&relative, &self.alloc);
      lit.raw = None;
    }
    walk_mut::walk_string_literal(self, lit);
  }
}
