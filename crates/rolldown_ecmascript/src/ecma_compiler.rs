use std::path::PathBuf;

use arcstr::ArcStr;
use oxc::{
  allocator::Allocator,
  ast::{
    ast::{Program, Statement},
    builder::AstBuilder,
  },
  codegen::{Codegen, CodegenOptions, CodegenReturn, CommentOptions, LegalComment},
  minifier::{ManglePropertyCache, Minifier, MinifierOptions},
  parser::{ParseOptions, Parser},
  span::{SPAN, SourceType},
};
use oxc_sourcemap::SourceMap;
use rolldown_error::{BuildDiagnostic, BuildResult, EventKind, Severity};

use crate::ecma_ast::{
  EcmaAst,
  program_cell::{ProgramCell, ProgramCellDependent, ProgramCellOwner},
};
pub struct EcmaCompiler;

impl EcmaCompiler {
  pub fn parse(id: &str, source: impl Into<ArcStr>, ty: SourceType) -> BuildResult<EcmaAst> {
    let source: ArcStr = source.into();
    let allocator = oxc::allocator::Allocator::default();
    let inner =
      ProgramCell::try_new(ProgramCellOwner { source: source.clone(), allocator }, |owner| {
        let parser = Parser::new(&owner.allocator, &owner.source, ty).with_options(ParseOptions {
          allow_return_outside_function: true,
          preserve_parens: false,
          ..ParseOptions::default()
        });
        let ret = parser.parse();
        if ret.fatal_error || !ret.diagnostics.is_empty() {
          Err(BuildDiagnostic::from_oxc_diagnostics(
            ret.diagnostics,
            &source.clone(),
            id,
            Severity::Error,
            EventKind::ParseError,
          ))
        } else {
          Ok(ProgramCellDependent { program: ret.program })
        }
      })?;
    Ok(EcmaAst { program: inner, source_type: ty })
  }

  pub fn parse_expr_as_program(
    id: &str,
    source: impl Into<ArcStr>,
    ty: SourceType,
  ) -> BuildResult<EcmaAst> {
    let source: ArcStr = source.into();
    let allocator = oxc::allocator::Allocator::default();
    let inner =
      ProgramCell::try_new(ProgramCellOwner { source: source.clone(), allocator }, |owner| {
        let builder = AstBuilder::new(&owner.allocator);
        let parser = Parser::new(&owner.allocator, &owner.source, ty)
          .with_options(ParseOptions { preserve_parens: false, ..ParseOptions::default() });
        let ret = parser.parse_expression();
        match ret {
          Ok(expr) => {
            let program = Program::new(
              SPAN,
              SourceType::default().with_module(true),
              owner.source.as_str(),
              [],
              None,
              [],
              [Statement::new_expression_statement(SPAN, expr, &builder)],
              &builder,
            );
            Ok(ProgramCellDependent { program })
          }
          Err(errors) => Err(BuildDiagnostic::from_oxc_diagnostics(
            errors,
            &source.clone(),
            id,
            Severity::Error,
            EventKind::ParseError,
          )),
        }
      })?;
    Ok(EcmaAst { program: inner, source_type: ty })
  }

  pub fn print_with(ast: &EcmaAst, options: PrintOptions) -> CodegenReturn<'_> {
    let legal = if options.comments.legal { LegalComment::Inline } else { LegalComment::None };
    Codegen::new()
      .with_options(CodegenOptions {
        comments: CommentOptions {
          normal: false,
          legal,
          jsdoc: options.comments.jsdoc,
          annotation: options.comments.annotation,
        },
        initial_indent: options.initial_indent,
        ascii_only: options.ascii_only,
        source_map_path: options.sourcemap.then(|| PathBuf::from(options.filename)),
        ..CodegenOptions::default()
      })
      .build(ast.program())
  }

  /// The returned map borrows `source_text` (its `sourcesContent` and token names are slices
  /// of it) — callers that keep it beyond the borrow must `into_owned` it; callers that only
  /// feed it to `collapse_sourcemaps` can use it as-is and skip that copy.
  /// The property-mangle cache is present when property mangling ran.
  #[expect(clippy::too_many_arguments)]
  pub fn dce_or_minify<'a>(
    allocator: &'a Allocator,
    source_text: &'a str,
    source_type: SourceType,
    enable_sourcemap: bool,
    filename: &str,
    compress: bool,
    minify_options: MinifierOptions,
    codegen_options: CodegenOptions,
  ) -> BuildResult<(String, Option<SourceMap<'a>>, Option<ManglePropertyCache>)> {
    // perf/bun-gap: RD_BG_TIMING=1 prints the parse / minify / codegen split per chunk.
    static BG_TIMING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let bg_timing =
      *BG_TIMING.get_or_init(|| std::env::var_os("RD_BG_TIMING").is_some_and(|v| v != "0"));
    let bg_t0 = std::time::Instant::now();
    static BG_PAR_CODEGEN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let bg_par_codegen = *BG_PAR_CODEGEN
      .get_or_init(|| std::env::var_os("RD_BG_PAR_CODEGEN").is_some_and(|v| v != "0"));
    let core = || -> BuildResult<(Program<'a>, oxc::minifier::MinifierReturn, std::time::Instant)> {
      let ret = Parser::new(allocator, source_text, source_type)
        .with_options(ParseOptions { preserve_parens: false, ..ParseOptions::default() })
        .parse();
      if ret.fatal_error || !ret.diagnostics.is_empty() {
        return Err(
          BuildDiagnostic::from_oxc_diagnostics(
            ret.diagnostics,
            &ArcStr::from(source_text),
            filename,
            Severity::Error,
            EventKind::ParseError,
          )
          .into(),
        );
      }
      let mut program = ret.program;
      let bg_t1 = std::time::Instant::now();
      static BG_MAX_ITER: std::sync::OnceLock<Option<u8>> = std::sync::OnceLock::new();
      let bg_max_iter = *BG_MAX_ITER
        .get_or_init(|| std::env::var("RD_BG_MAX_ITER").ok().and_then(|v| v.parse::<u8>().ok()));
      let ret = if compress
        && (bg_timing || bg_max_iter.is_some())
        && minify_options.mangle_properties.is_none()
        && minify_options.compress.is_some()
        && minify_options.mangle.is_some()
      {
        bg_minify_split(allocator, &mut program, minify_options, bg_max_iter, filename, bg_timing)
      } else {
        let minifier = Minifier::new(minify_options);
        if compress { minifier.minify(allocator, &mut program) } else { minifier.dce(allocator, &mut program) }
      };
      Ok((program, ret, bg_t1))
    };
    // perf/bun-gap A3: build the sourcemap line tables of the chunk text on another thread while
    // this one parses and minifies.
    let mut bg_line_tables = None;
    let (mut program, ret, bg_t1) = if bg_par_codegen && enable_sourcemap {
      let tables = &mut bg_line_tables;
      rayon::in_place_scope(|s| {
        s.spawn(move |_| *tables = Some(oxc::codegen::generate_line_offset_tables(source_text)));
        core()
      })?
    } else {
      core()?
    };
    let property_mangle_cache = ret.property_mangle_cache;
    let bg_t2 = std::time::Instant::now();
    if bg_par_codegen {
      let opts = CodegenOptions {
        source_map_path: enable_sourcemap.then(|| PathBuf::from(filename)),
        ..codegen_options.clone()
      };
      if let Some((code, map)) = crate::bg_par_codegen::par_codegen(
        allocator,
        &mut program,
        &opts,
        ret.scoping.as_ref(),
        ret.class_private_mappings.as_ref(),
        bg_line_tables,
      ) {
        if bg_timing {
          #[expect(clippy::print_stderr, reason = "perf investigation output")]
          {
            eprintln!(
              "[bg] dce_or_minify {filename} bytes={} parse={:.3} minify={:.3} codegen(par)={:.3}",
              source_text.len(),
              (bg_t1 - bg_t0).as_secs_f64() * 1000.0,
              (bg_t2 - bg_t1).as_secs_f64() * 1000.0,
              bg_t2.elapsed().as_secs_f64() * 1000.0
            );
          }
        }
        return Ok((code, map, property_mangle_cache));
      }
    }
    let ret = Codegen::new()
      .with_options(CodegenOptions {
        source_map_path: enable_sourcemap.then(|| PathBuf::from(filename)),
        ..codegen_options
      })
      .with_scoping(ret.scoping)
      .with_private_member_mappings(ret.class_private_mappings)
      .build(&program);
    if bg_timing {
      #[expect(clippy::print_stderr, reason = "perf investigation output")]
      {
        eprintln!(
          "[bg] dce_or_minify {filename} bytes={} parse={:.3} minify={:.3} codegen={:.3}",
          source_text.len(),
          (bg_t1 - bg_t0).as_secs_f64() * 1000.0,
          (bg_t2 - bg_t1).as_secs_f64() * 1000.0,
          bg_t2.elapsed().as_secs_f64() * 1000.0
        );
      }
    }
    Ok((ret.code, ret.map, property_mangle_cache))
  }
}

/// perf/bun-gap: the same steps as `oxc_minifier::Minifier::build` (Full mode, no property
/// mangling), timed step by step. `max_iter` caps the compressor's fixed-point loop.
fn bg_minify_split<'a>(
  allocator: &'a Allocator,
  program: &mut Program<'a>,
  options: MinifierOptions,
  max_iter: Option<u8>,
  filename: &str,
  timing: bool,
) -> oxc::minifier::MinifierReturn {
  use oxc::{mangler::Mangler, minifier::Compressor, semantic::SemanticBuilder};
  let t0 = std::time::Instant::now();
  let semantic = SemanticBuilder::new().build(program).semantic;
  let stats = semantic.stats();
  let scoping = semantic.into_scoping();
  let t1 = std::time::Instant::now();
  let mut compress_options = options.compress.expect("compress");
  if max_iter.is_some() {
    compress_options.max_iterations = max_iter;
  }
  let iterations = Compressor::new(allocator).build_with_scoping(program, scoping, compress_options);
  let t2 = std::time::Instant::now();
  let mut semantic = SemanticBuilder::new()
    .with_build_nodes(true)
    .with_class_table(true)
    .with_stats(stats)
    .build(program)
    .semantic;
  let t3 = std::time::Instant::now();
  let class_private_mappings = Mangler::default()
    .with_options(options.mangle.expect("mangle"))
    .build_with_semantic(&mut semantic, program);
  let t4 = std::time::Instant::now();
  if timing {
    let ms = |a: std::time::Instant, b: std::time::Instant| (b - a).as_secs_f64() * 1000.0;
    #[expect(clippy::print_stderr, reason = "perf investigation output")]
    {
      eprintln!(
        "[bg] minify_split {filename} semantic1={:.3} compress={:.3} iterations={iterations} semantic2={:.3} mangle={:.3}",
        ms(t0, t1),
        ms(t1, t2),
        ms(t2, t3),
        ms(t3, t4)
      );
    }
  }
  oxc::minifier::MinifierReturn {
    scoping: Some(semantic.into_scoping()),
    class_private_mappings: Some(class_private_mappings),
    property_mangle_cache: None,
    iterations,
  }
}

#[test]
fn basic_test() {
  let ast = EcmaCompiler::parse("", "const a = 1;".to_string(), SourceType::default()).unwrap();
  let code = EcmaCompiler::print_with(&ast, PrintOptions::default()).code;
  assert_eq!(code, "const a = 1;\n");
}
#[derive(Debug, Default)]

pub struct PrintOptions {
  pub comments: PrintCommentsOptions,
  pub filename: String,
  pub sourcemap: bool,
  pub initial_indent: u32,
  pub ascii_only: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PrintCommentsOptions {
  pub legal: bool,
  pub annotation: bool,
  pub jsdoc: bool,
}
