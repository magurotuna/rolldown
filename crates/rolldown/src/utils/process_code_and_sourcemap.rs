use std::path::Path;

use oxc::ast::CommentKind;
use rolldown_common::{NormalizedBundlerOptions, OutputAsset, SourceMapType};
use rolldown_error::{BuildResult, ResultExt};
use rolldown_sourcemap::SourceMap;
use rolldown_std_utils::relative_path_to_slash;
use rolldown_utils::rayon::{IntoParallelIterator, ParallelIterator};
use sugar_path::SugarPath;
use url::Url;

use super::uuid::uuid_v4_string_from_u128;

#[expect(clippy::too_many_arguments)]
#[cfg(feature = "experimental")]
pub async fn process_code_and_sourcemap(
  options: &NormalizedBundlerOptions,
  code: &mut String,
  map: &mut SourceMap,
  file_dir: &Path,
  filename: &str,
  debug_id: u128,
  is_css: bool,
  sourcemap_filename: Option<String>,
) -> BuildResult<Option<OutputAsset>> {
  prepare_sourcemap(options, map, file_dir, filename).await?;
  let map_filename = sourcemap_filename.unwrap_or_else(|| format!("{filename}.map"));
  emit_sourcemap(options, code, map, &map_filename, debug_id, is_css)
}

pub async fn prepare_sourcemap(
  options: &NormalizedBundlerOptions,
  map: &mut SourceMap,
  file_dir: &Path,
  filename: &str,
) -> BuildResult<()> {
  let file_base_name = Path::new(filename).file_name().expect("should have file name");
  map.set_file(file_base_name.to_string_lossy().as_ref());

  if options.sourcemap_exclude_sources {
    map.set_source_contents(vec![]);
  }

  let map_filename = format!("{}.map", file_base_name.to_string_lossy());
  let map_path = file_dir.join(&map_filename);

  // The ignore list, the path transform and the final `sources` all start from the sources
  // relative to `file_dir`, so compute them once. Windows still builds `sources` with slashes below.
  let relative_sources = if options.sourcemap_ignore_list.is_some()
    || options.sourcemap_path_transform.is_some()
    || !cfg!(windows)
  {
    relative_sources(map, file_dir)
  } else {
    vec![]
  };

  if let Some(source_map_ignore_list) = &options.sourcemap_ignore_list {
    let ignored = match source_map_ignore_list {
      rolldown_common::SourceMapIgnoreList::Boolean(_)
      | rolldown_common::SourceMapIgnoreList::StringOrRegex(_) => {
        // Fast path: no async overhead for static values (boolean/string/regex)
        relative_sources
          .iter()
          .map(|source| source_map_ignore_list.exec_static(source))
          .collect::<Vec<_>>()
      }
      rolldown_common::SourceMapIgnoreList::Fn(_) => {
        // Slow path: one call decides every source, so the napi boundary is crossed once per
        // sourcemap instead of once per source.
        let source_count = relative_sources.len();
        let ignored = source_map_ignore_list
          .exec_dynamic(relative_sources.clone(), map_path.to_string_lossy().as_ref())
          .await?;
        if ignored.len() != source_count {
          return Err(
            anyhow::anyhow!(
              "`sourcemapIgnoreList` returned {} results for {} sources",
              ignored.len(),
              source_count
            )
            .into(),
          );
        }
        ignored
      }
    };

    #[expect(clippy::cast_possible_truncation)]
    let ignore_list = ignored
      .into_iter()
      .enumerate()
      .filter_map(|(index, should_ignore)| should_ignore.then_some(index as u32))
      .collect::<Vec<_>>();

    if !ignore_list.is_empty() {
      map.set_ignore_list(ignore_list);
    }
  }

  if let Some(sourcemap_path_transform) = &options.sourcemap_path_transform {
    let map_path = map_path.to_string_lossy();
    let source_count = relative_sources.len();

    // One call rewrites every source, so the napi boundary is crossed once per sourcemap instead
    // of once per source.
    let sources = sourcemap_path_transform.call(relative_sources, map_path.as_ref()).await?;
    if sources.len() != source_count {
      return Err(
        anyhow::anyhow!(
          "`sourcemapPathTransform` returned {} results for {} sources",
          sources.len(),
          source_count
        )
        .into(),
      );
    }

    #[cfg(windows)]
    // Normalize the windows path.
    let sources = sources
      .into_iter()
      .map(|source| source.replace(std::path::MAIN_SEPARATOR, "/"))
      .collect::<Vec<_>>();

    map.set_sources(sources);
  } else if cfg!(windows) {
    // Normalize the windows path at final.
    let sources =
      map.get_sources().map(|x| relative_path_to_slash(x.as_path(), file_dir)).collect::<Vec<_>>();
    map.set_sources(sources);
  } else {
    map.set_sources(relative_sources);
  }

  Ok(())
}

/// Maps with at least this many sources compute their relative paths in parallel. Below it, rayon's
/// overhead outweighed the gain in a build with many small chunk maps.
const PARALLEL_RELATIVE_SOURCES_THRESHOLD: usize = 1024;

fn relative_sources(map: &SourceMap, file_dir: &Path) -> Vec<String> {
  let relative = |source: &str| source.as_path().relative(file_dir).to_string_lossy().into_owned();
  if map.get_sources().len() >= PARALLEL_RELATIVE_SOURCES_THRESHOLD {
    map.get_sources().collect::<Vec<_>>().into_par_iter().map(relative).collect()
  } else {
    map.get_sources().map(relative).collect()
  }
}

pub fn emit_sourcemap(
  options: &NormalizedBundlerOptions,
  code: &mut String,
  map: &mut SourceMap,
  map_filename: &str,
  debug_id: u128,
  is_css: bool,
) -> BuildResult<Option<OutputAsset>> {
  let source_map_link_comment_kind =
    if is_css { CommentKind::SingleLineBlock } else { CommentKind::Line };

  if options.sourcemap_debug_ids && options.sourcemap.is_some() {
    let debug_id_str = uuid_v4_string_from_u128(debug_id);
    map.set_debug_id(&debug_id_str);

    process_sourcemap_related_reference(
      code,
      |source| {
        source.push_str("# debugId=");
        source.push_str(debug_id_str.as_str());
        Ok(())
      },
      source_map_link_comment_kind,
    )?;
  }

  if let Some(sourcemap) = &options.sourcemap {
    match sourcemap {
      SourceMapType::File | SourceMapType::Hidden => {
        let source = map.to_json_string();
        if matches!(sourcemap, SourceMapType::File) {
          process_sourcemap_related_reference(
            code,
            |source| {
              source.push_str("# sourceMappingURL=");

              match &options.sourcemap_base_url {
                Some(url_string) => {
                  let url = Url::parse(url_string)
                    .and_then(|base| base.join(map_filename))
                    .map_err_to_unhandleable()?;
                  source.push_str(url.as_str());
                }
                None => {
                  source.push_str(
                    &Path::new(map_filename)
                      .file_name()
                      .ok_or(anyhow::anyhow!("should have filename"))?
                      .to_string_lossy(),
                  );
                }
              }
              Ok(())
            },
            source_map_link_comment_kind,
          )?;
        }
        return Ok(Some(OutputAsset {
          filename: map_filename.into(),
          source: source.into(),
          original_file_names: vec![],
          names: vec![],
        }));
      }
      SourceMapType::Inline => {
        let data_url = map.to_data_url();
        process_sourcemap_related_reference(
          code,
          |source| {
            source.push_str("# sourceMappingURL=");
            source.push_str(&data_url);
            Ok(())
          },
          source_map_link_comment_kind,
        )?;
      }
    }
  }

  Ok(None)
}

fn process_sourcemap_related_reference(
  source: &mut String,
  mut reference_body_processor: impl FnMut(&mut String) -> BuildResult<()>,
  comment_kind: CommentKind,
) -> BuildResult<()> {
  source.push('\n');
  match comment_kind {
    CommentKind::Line | CommentKind::HtmlOpen | CommentKind::HtmlClose => {
      source.push_str("//");
      reference_body_processor(source)?;
    }
    CommentKind::SingleLineBlock | CommentKind::MultiLineBlock => {
      source.push_str("/*");
      reference_body_processor(source)?;
      source.push_str("*/");
    }
  }
  Ok(())
}

#[cfg(test)]
mod test {
  use std::{borrow::Cow, path::Path};

  use rolldown_sourcemap::SourceMap;
  use sugar_path::SugarPath;

  use super::{PARALLEL_RELATIVE_SOURCES_THRESHOLD, relative_sources};

  #[test]
  fn relative_sources_in_parallel_keep_the_source_order() {
    // Enough sources to take the parallel path, whose result must match the serial one in order.
    let count = PARALLEL_RELATIVE_SOURCES_THRESHOLD + 7;
    let sources =
      (0..count).map(|i| Cow::Owned(format!("/project/src/dir{}/module{i}.js", i % 13))).collect();
    let map = SourceMap::new(None, vec![], None, sources, vec![], Box::new([]), None);
    let file_dir = Path::new("/project/dist/assets");
    let serial = map
      .get_sources()
      .map(|source| source.as_path().relative(file_dir).to_string_lossy().into_owned())
      .collect::<Vec<_>>();
    assert_eq!(serial.len(), count);
    assert_eq!(relative_sources(&map, file_dir), serial);
  }
}
