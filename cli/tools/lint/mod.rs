// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

//! This module provides file linting utilities using
//! [`deno_lint`](https://github.com/denoland/deno_lint).
use deno_ast::diagnostics::Diagnostic;
use deno_ast::MediaType;
use deno_ast::ModuleSpecifier;
use deno_ast::ParsedSource;
use deno_ast::SourceRange;
use deno_ast::SourceTextInfo;
use deno_config::glob::FilePatterns;
use deno_core::anyhow::bail;
use deno_core::anyhow::Context;
use deno_core::error::generic_error;
use deno_core::error::AnyError;
use deno_core::parking_lot::Mutex;
use deno_core::serde_json;
use deno_graph::FastCheckDiagnostic;
use deno_lint::diagnostic::LintDiagnostic;
use deno_lint::linter::LintFileOptions;
use deno_lint::linter::Linter;
use deno_lint::linter::LinterBuilder;
use deno_lint::rules;
use deno_lint::rules::LintRule;
use log::debug;
use log::info;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashSet;
use std::fs;
use std::io::stdin;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use crate::args::Flags;
use crate::args::LintFlags;
use crate::args::LintOptions;
use crate::args::LintReporterKind;
use crate::args::LintRulesConfig;
use crate::cache::IncrementalCache;
use crate::colors;
use crate::factory::CliFactory;
use crate::tools::fmt::run_parallelized;
use crate::util::file_watcher;
use crate::util::fs::canonicalize_path;
use crate::util::fs::specifier_from_file_path;
use crate::util::fs::FileCollector;
use crate::util::path::is_script_ext;
use crate::util::sync::AtomicFlag;

pub mod no_slow_types;

static STDIN_FILE_NAME: &str = "$deno$stdin.ts";

fn create_reporter(kind: LintReporterKind) -> Box<dyn LintReporter + Send> {
  match kind {
    LintReporterKind::Pretty => Box::new(PrettyLintReporter::new()),
    LintReporterKind::Json => Box::new(JsonLintReporter::new()),
    LintReporterKind::Compact => Box::new(CompactLintReporter::new()),
  }
}

pub async fn lint(flags: Flags, lint_flags: LintFlags) -> Result<(), AnyError> {
  if let Some(watch_flags) = &lint_flags.watch {
    if lint_flags.is_stdin() {
      return Err(generic_error(
        "Lint watch on standard input is not supported.",
      ));
    }
    file_watcher::watch_func(
      flags,
      file_watcher::PrintConfig::new("Lint", !watch_flags.no_clear_screen),
      move |flags, watcher_communicator, changed_paths| {
        let lint_flags = lint_flags.clone();
        Ok(async move {
          let factory = CliFactory::from_flags(flags)?;
          let cli_options = factory.cli_options();
          let lint_options = cli_options.resolve_lint_options(lint_flags)?;
          let files = collect_lint_files(lint_options.files.clone()).and_then(
            |files| {
              if files.is_empty() {
                Err(generic_error("No target files found."))
              } else {
                Ok(files)
              }
            },
          )?;
          _ = watcher_communicator.watch_paths(files.clone());

          let lint_paths = if let Some(paths) = changed_paths {
            // lint all files on any changed (https://github.com/denoland/deno/issues/12446)
            files
              .iter()
              .any(|path| {
                canonicalize_path(path)
                  .map(|p| paths.contains(&p))
                  .unwrap_or(false)
              })
              .then_some(files)
              .unwrap_or_else(|| [].to_vec())
          } else {
            files
          };

          lint_files(factory, lint_options, lint_paths).await?;
          Ok(())
        })
      },
    )
    .await?;
  } else {
    let factory = CliFactory::from_flags(flags)?;
    let cli_options = factory.cli_options();
    let is_stdin = lint_flags.is_stdin();
    let lint_options = cli_options.resolve_lint_options(lint_flags)?;
    let files = &lint_options.files;
    let success = if is_stdin {
      let reporter_kind = lint_options.reporter_kind;
      let reporter_lock = Arc::new(Mutex::new(create_reporter(reporter_kind)));
      let lint_rules = get_config_rules_err_empty(
        lint_options.rules,
        cli_options.maybe_config_file().as_ref(),
      )?;
      let file_path = cli_options.initial_cwd().join(STDIN_FILE_NAME);
      let r = lint_stdin(&file_path, lint_rules.rules);
      let success = handle_lint_result(
        &file_path.to_string_lossy(),
        r,
        reporter_lock.clone(),
      );
      reporter_lock.lock().close(1);
      success
    } else {
      let target_files =
        collect_lint_files(files.clone()).and_then(|files| {
          if files.is_empty() {
            Err(generic_error("No target files found."))
          } else {
            Ok(files)
          }
        })?;
      debug!("Found {} files", target_files.len());
      lint_files(factory, lint_options, target_files).await?
    };
    if !success {
      std::process::exit(1);
    }
  }

  Ok(())
}

async fn lint_files(
  factory: CliFactory,
  lint_options: LintOptions,
  paths: Vec<PathBuf>,
) -> Result<bool, AnyError> {
  let caches = factory.caches()?;
  let maybe_config_file = factory.cli_options().maybe_config_file().as_ref();
  let lint_rules =
    get_config_rules_err_empty(lint_options.rules, maybe_config_file)?;
  let incremental_cache = Arc::new(IncrementalCache::new(
    caches.lint_incremental_cache_db(),
    &lint_rules.incremental_cache_state(),
    &paths,
  ));
  let target_files_len = paths.len();
  let reporter_kind = lint_options.reporter_kind;
  // todo(dsherret): abstract away this lock behind a performant interface
  let reporter_lock =
    Arc::new(Mutex::new(create_reporter(reporter_kind.clone())));
  let has_error = Arc::new(AtomicFlag::default());

  let mut futures = Vec::with_capacity(2);
  if lint_rules.no_slow_types {
    if let Some(config_file) = maybe_config_file {
      let members = config_file.to_workspace_members()?;
      let has_error = has_error.clone();
      let reporter_lock = reporter_lock.clone();
      let module_graph_creator = factory.module_graph_creator().await?.clone();
      let path_urls = paths
        .iter()
        .filter_map(|p| ModuleSpecifier::from_file_path(p).ok())
        .collect::<HashSet<_>>();
      futures.push(deno_core::unsync::spawn(async move {
        let graph = module_graph_creator
          .create_and_validate_publish_graph(&members, true)
          .await?;
        // todo(dsherret): this isn't exactly correct as linting isn't properly
        // setup to handle workspaces. Iterating over the workspace members
        // should be done at a higher level because it also needs to take into
        // account the config per workspace member.
        for member in &members {
          let export_urls = member.config_file.resolve_export_value_urls()?;
          if !export_urls.iter().any(|url| path_urls.contains(url)) {
            continue; // entrypoint is not specified, so skip
          }
          let diagnostics = no_slow_types::collect_no_slow_type_diagnostics(
            &export_urls,
            &graph,
          );
          if !diagnostics.is_empty() {
            has_error.raise();
            let mut reporter = reporter_lock.lock();
            for diagnostic in &diagnostics {
              reporter
                .visit_diagnostic(LintOrCliDiagnostic::FastCheck(diagnostic));
            }
          }
        }
        Ok(())
      }));
    }
  }

  futures.push({
    let has_error = has_error.clone();
    let linter = create_linter(lint_rules.rules);
    let reporter_lock = reporter_lock.clone();
    let incremental_cache = incremental_cache.clone();
    let fix = lint_options.fix;
    deno_core::unsync::spawn(async move {
      run_parallelized(paths, {
        move |file_path| {
          let file_text = fs::read_to_string(&file_path)?;

          // don't bother rechecking this file if it didn't have any diagnostics before
          if incremental_cache.is_file_same(&file_path, &file_text) {
            return Ok(());
          }

          let r = lint_file(&linter, &file_path, file_text, fix);
          if let Ok((file_source, file_diagnostics)) = &r {
            if file_diagnostics.is_empty() {
              // update the incremental cache if there were no diagnostics
              incremental_cache.update_file(
                &file_path,
                // ensure the returned text is used here as it may have been modified via --fix
                file_source.text_info().text_str(),
              )
            }
          }

          let success = handle_lint_result(
            &file_path.to_string_lossy(),
            r,
            reporter_lock.clone(),
          );
          if !success {
            has_error.raise();
          }

          Ok(())
        }
      })
      .await
    })
  });

  deno_core::futures::future::try_join_all(futures).await?;

  incremental_cache.wait_completion().await;
  reporter_lock.lock().close(target_files_len);

  Ok(!has_error.is_raised())
}

fn collect_lint_files(files: FilePatterns) -> Result<Vec<PathBuf>, AnyError> {
  FileCollector::new(|e| is_script_ext(e.path))
    .ignore_git_folder()
    .ignore_node_modules()
    .ignore_vendor_folder()
    .collect_file_patterns(files)
}

pub fn print_rules_list(json: bool, maybe_rules_tags: Option<Vec<String>>) {
  let lint_rules = if maybe_rules_tags.is_none() {
    rules::get_all_rules()
  } else {
    rules::get_filtered_rules(maybe_rules_tags, None, None)
  };

  if json {
    let json_rules: Vec<serde_json::Value> = lint_rules
      .iter()
      .map(|rule| {
        serde_json::json!({
          "code": rule.code(),
          "tags": rule.tags(),
          "docs": rule.docs(),
        })
      })
      .collect();
    let json_str = serde_json::to_string_pretty(&json_rules).unwrap();
    println!("{json_str}");
  } else {
    // The rules should still be printed even if `--quiet` option is enabled,
    // so use `println!` here instead of `info!`.
    println!("Available rules:");
    for rule in lint_rules.iter() {
      print!(" - {}", colors::cyan(rule.code()));
      if rule.tags().is_empty() {
        println!();
      } else {
        println!(" [{}]", colors::gray(rule.tags().join(", ")))
      }
      println!(
        "{}",
        colors::gray(format!(
          "   help: https://lint.deno.land/#{}",
          rule.code()
        ))
      );
      println!();
    }
  }
}

pub fn create_linter(rules: Vec<&'static dyn LintRule>) -> Linter {
  LinterBuilder::default()
    .ignore_file_directive("deno-lint-ignore-file")
    .ignore_diagnostic_directive("deno-lint-ignore")
    .rules(rules)
    .build()
}

fn lint_file(
  linter: &Linter,
  file_path: &Path,
  source_code: String,
  fix: bool,
) -> Result<(ParsedSource, Vec<LintDiagnostic>), AnyError> {
  let specifier = specifier_from_file_path(file_path)?;
  let media_type = MediaType::from_specifier(&specifier);

  if fix {
    lint_file_and_fix(linter, &specifier, media_type, source_code, file_path)
  } else {
    linter
      .lint_file(LintFileOptions {
        specifier,
        media_type,
        source_code,
      })
      .map_err(AnyError::from)
  }
}

fn lint_file_and_fix(
  linter: &Linter,
  specifier: &ModuleSpecifier,
  media_type: MediaType,
  source_code: String,
  file_path: &Path,
) -> Result<(ParsedSource, Vec<LintDiagnostic>), deno_core::anyhow::Error> {
  // initial lint
  let (source, diagnostics) = linter.lint_file(LintFileOptions {
    specifier: specifier.clone(),
    media_type,
    source_code,
  })?;

  // Try applying fixes repeatedly until the file has none left or
  // a maximum number of iterations is reached. This is necessary
  // because lint fixes may overlap and so we can't always apply
  // them in one pass.
  let mut source = source;
  let mut diagnostics = diagnostics;
  let mut fix_iterations = 0;
  loop {
    let change = apply_lint_fixes_and_relint(
      specifier,
      media_type,
      linter,
      source.text_info(),
      &diagnostics,
    )?;
    match change {
      Some(change) => {
        source = change.0;
        diagnostics = change.1;
      }
      None => {
        break;
      }
    }
    fix_iterations += 1;
    if fix_iterations > 5 {
      log::warn!(
        concat!(
          "Reached maximum number of fix iterations for '{}'. There's ",
          "probably a bug in Deno. Please fix this file manually.",
        ),
        specifier,
      );
      break;
    }
  }

  if fix_iterations > 0 {
    // everything looks good and the file still parses, so write it out
    fs::write(file_path, source.text_info().text_str())
      .context("Failed writing fix to file.")?;
  }

  Ok((source, diagnostics))
}

fn apply_lint_fixes_and_relint(
  specifier: &ModuleSpecifier,
  media_type: MediaType,
  linter: &Linter,
  text_info: &SourceTextInfo,
  diagnostics: &[LintDiagnostic],
) -> Result<Option<(ParsedSource, Vec<LintDiagnostic>)>, AnyError> {
  let Some(new_text) = apply_lint_fixes(text_info, diagnostics) else {
    return Ok(None);
  };
  linter
    .lint_file(LintFileOptions {
      specifier: specifier.clone(),
      source_code: new_text,
      media_type,
    })
    .map(Some)
    .context(
      "An applied lint fix caused a syntax error. Please report this bug.",
    )
}

fn apply_lint_fixes(
  text_info: &SourceTextInfo,
  diagnostics: &[LintDiagnostic],
) -> Option<String> {
  if diagnostics.is_empty() {
    return None;
  }

  let file_start = text_info.range().start;
  let mut quick_fixes = diagnostics
    .iter()
    // use the first quick fix
    .filter_map(|d| d.fixes.first())
    .flat_map(|fix| fix.changes.iter())
    .map(|change| deno_ast::TextChange {
      range: change.range.as_byte_range(file_start),
      new_text: change.new_text.to_string(),
    })
    .collect::<Vec<_>>();
  if quick_fixes.is_empty() {
    return None;
  }
  // remove any overlapping text changes, we'll circle
  // back for another pass to fix the remaining
  quick_fixes.sort_by_key(|change| change.range.start);
  for i in (1..quick_fixes.len()).rev() {
    let cur = &quick_fixes[i];
    let previous = &quick_fixes[i - 1];
    let is_overlapping = cur.range.start < previous.range.end;
    if is_overlapping {
      quick_fixes.remove(i);
    }
  }
  let new_text =
    deno_ast::apply_text_changes(text_info.text_str(), quick_fixes);
  Some(new_text)
}

/// Lint stdin and write result to stdout.
/// Treats input as TypeScript.
/// Compatible with `--json` flag.
fn lint_stdin(
  file_path: &Path,
  lint_rules: Vec<&'static dyn LintRule>,
) -> Result<(ParsedSource, Vec<LintDiagnostic>), AnyError> {
  let mut source_code = String::new();
  if stdin().read_to_string(&mut source_code).is_err() {
    return Err(generic_error("Failed to read from stdin"));
  }

  let linter = create_linter(lint_rules);

  linter
    .lint_file(LintFileOptions {
      specifier: specifier_from_file_path(file_path)?,
      source_code: source_code.clone(),
      media_type: MediaType::TypeScript,
    })
    .map_err(AnyError::from)
}

fn handle_lint_result(
  file_path: &str,
  result: Result<(ParsedSource, Vec<LintDiagnostic>), AnyError>,
  reporter_lock: Arc<Mutex<Box<dyn LintReporter + Send>>>,
) -> bool {
  let mut reporter = reporter_lock.lock();

  match result {
    Ok((_source, mut file_diagnostics)) => {
      file_diagnostics.sort_by(|a, b| match a.specifier.cmp(&b.specifier) {
        std::cmp::Ordering::Equal => a.range.start.cmp(&b.range.start),
        file_order => file_order,
      });
      for d in &file_diagnostics {
        reporter.visit_diagnostic(LintOrCliDiagnostic::Lint(d));
      }
      file_diagnostics.is_empty()
    }
    Err(err) => {
      reporter.visit_error(file_path, &err);
      false
    }
  }
}

#[derive(Clone, Copy)]
pub enum LintOrCliDiagnostic<'a> {
  Lint(&'a LintDiagnostic),
  FastCheck(&'a FastCheckDiagnostic),
}

impl<'a> LintOrCliDiagnostic<'a> {
  pub fn specifier(&self) -> &ModuleSpecifier {
    match self {
      LintOrCliDiagnostic::Lint(d) => &d.specifier,
      LintOrCliDiagnostic::FastCheck(d) => d.specifier(),
    }
  }

  pub fn range(&self) -> Option<(&SourceTextInfo, SourceRange)> {
    match self {
      LintOrCliDiagnostic::Lint(d) => Some((&d.text_info, d.range)),
      LintOrCliDiagnostic::FastCheck(d) => {
        d.range().map(|r| (&r.text_info, r.range))
      }
    }
  }
}

impl<'a> deno_ast::diagnostics::Diagnostic for LintOrCliDiagnostic<'a> {
  fn level(&self) -> deno_ast::diagnostics::DiagnosticLevel {
    match self {
      LintOrCliDiagnostic::Lint(d) => d.level(),
      LintOrCliDiagnostic::FastCheck(d) => d.level(),
    }
  }

  fn code(&self) -> Cow<'_, str> {
    match self {
      LintOrCliDiagnostic::Lint(d) => d.code(),
      LintOrCliDiagnostic::FastCheck(_) => Cow::Borrowed("no-slow-types"),
    }
  }

  fn message(&self) -> Cow<'_, str> {
    match self {
      LintOrCliDiagnostic::Lint(d) => d.message(),
      LintOrCliDiagnostic::FastCheck(d) => d.message(),
    }
  }

  fn location(&self) -> deno_ast::diagnostics::DiagnosticLocation {
    match self {
      LintOrCliDiagnostic::Lint(d) => d.location(),
      LintOrCliDiagnostic::FastCheck(d) => d.location(),
    }
  }

  fn snippet(&self) -> Option<deno_ast::diagnostics::DiagnosticSnippet<'_>> {
    match self {
      LintOrCliDiagnostic::Lint(d) => d.snippet(),
      LintOrCliDiagnostic::FastCheck(d) => d.snippet(),
    }
  }

  fn hint(&self) -> Option<Cow<'_, str>> {
    match self {
      LintOrCliDiagnostic::Lint(d) => d.hint(),
      LintOrCliDiagnostic::FastCheck(d) => d.hint(),
    }
  }

  fn snippet_fixed(
    &self,
  ) -> Option<deno_ast::diagnostics::DiagnosticSnippet<'_>> {
    match self {
      LintOrCliDiagnostic::Lint(d) => d.snippet_fixed(),
      LintOrCliDiagnostic::FastCheck(d) => d.snippet_fixed(),
    }
  }

  fn info(&self) -> Cow<'_, [Cow<'_, str>]> {
    match self {
      LintOrCliDiagnostic::Lint(d) => d.info(),
      LintOrCliDiagnostic::FastCheck(d) => d.info(),
    }
  }

  fn docs_url(&self) -> Option<Cow<'_, str>> {
    match self {
      LintOrCliDiagnostic::Lint(d) => d.docs_url(),
      LintOrCliDiagnostic::FastCheck(d) => d.docs_url(),
    }
  }
}

trait LintReporter {
  fn visit_diagnostic(&mut self, d: LintOrCliDiagnostic);
  fn visit_error(&mut self, file_path: &str, err: &AnyError);
  fn close(&mut self, check_count: usize);
}

#[derive(Serialize)]
struct LintError {
  file_path: String,
  message: String,
}

struct PrettyLintReporter {
  lint_count: u32,
  fixable_diagnostics: u32,
}

impl PrettyLintReporter {
  fn new() -> PrettyLintReporter {
    PrettyLintReporter {
      lint_count: 0,
      fixable_diagnostics: 0,
    }
  }
}

impl LintReporter for PrettyLintReporter {
  fn visit_diagnostic(&mut self, d: LintOrCliDiagnostic) {
    self.lint_count += 1;
    if let LintOrCliDiagnostic::Lint(d) = d {
      if !d.fixes.is_empty() {
        self.fixable_diagnostics += 1;
      }
    }

    eprintln!("{}", d.display());
  }

  fn visit_error(&mut self, file_path: &str, err: &AnyError) {
    eprintln!("Error linting: {file_path}");
    eprintln!("   {err}");
  }

  fn close(&mut self, check_count: usize) {
    let fixable_suffix = if self.fixable_diagnostics > 0 {
      colors::gray(format!(" ({} fixable via --fix)", self.fixable_diagnostics))
        .to_string()
    } else {
      "".to_string()
    };
    match self.lint_count {
      1 => info!("Found 1 problem{}", fixable_suffix),
      n if n > 1 => {
        info!("Found {} problems{}", self.lint_count, fixable_suffix)
      }
      _ => (),
    }

    match check_count {
      n if n <= 1 => info!("Checked {} file", n),
      n if n > 1 => info!("Checked {} files", n),
      _ => unreachable!(),
    }
  }
}

struct CompactLintReporter {
  lint_count: u32,
}

impl CompactLintReporter {
  fn new() -> CompactLintReporter {
    CompactLintReporter { lint_count: 0 }
  }
}

impl LintReporter for CompactLintReporter {
  fn visit_diagnostic(&mut self, d: LintOrCliDiagnostic) {
    self.lint_count += 1;

    match d.range() {
      Some((text_info, range)) => {
        let line_and_column = text_info.line_and_column_display(range.start);
        eprintln!(
          "{}: line {}, col {} - {} ({})",
          d.specifier(),
          line_and_column.line_number,
          line_and_column.column_number,
          d.message(),
          d.code(),
        )
      }
      None => {
        eprintln!("{}: {} ({})", d.specifier(), d.message(), d.code())
      }
    }
  }

  fn visit_error(&mut self, file_path: &str, err: &AnyError) {
    eprintln!("Error linting: {file_path}");
    eprintln!("   {err}");
  }

  fn close(&mut self, check_count: usize) {
    match self.lint_count {
      1 => info!("Found 1 problem"),
      n if n > 1 => info!("Found {} problems", self.lint_count),
      _ => (),
    }

    match check_count {
      n if n <= 1 => info!("Checked {} file", n),
      n if n > 1 => info!("Checked {} files", n),
      _ => unreachable!(),
    }
  }
}

// WARNING: Ensure doesn't change because it's used in the JSON output
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonDiagnosticLintPosition {
  /// The 1-indexed line number.
  pub line: usize,
  /// The 0-indexed column index.
  pub col: usize,
  pub byte_pos: usize,
}

impl JsonDiagnosticLintPosition {
  pub fn new(byte_index: usize, loc: deno_ast::LineAndColumnIndex) -> Self {
    JsonDiagnosticLintPosition {
      line: loc.line_index + 1,
      col: loc.column_index,
      byte_pos: byte_index,
    }
  }
}

// WARNING: Ensure doesn't change because it's used in the JSON output
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct JsonLintDiagnosticRange {
  pub start: JsonDiagnosticLintPosition,
  pub end: JsonDiagnosticLintPosition,
}

// WARNING: Ensure doesn't change because it's used in the JSON output
#[derive(Clone, Serialize)]
struct JsonLintDiagnostic {
  pub filename: String,
  pub range: Option<JsonLintDiagnosticRange>,
  pub message: String,
  pub code: String,
  pub hint: Option<String>,
}

#[derive(Serialize)]
struct JsonLintReporter {
  diagnostics: Vec<JsonLintDiagnostic>,
  errors: Vec<LintError>,
}

impl JsonLintReporter {
  fn new() -> JsonLintReporter {
    JsonLintReporter {
      diagnostics: Vec::new(),
      errors: Vec::new(),
    }
  }
}

impl LintReporter for JsonLintReporter {
  fn visit_diagnostic(&mut self, d: LintOrCliDiagnostic) {
    self.diagnostics.push(JsonLintDiagnostic {
      filename: d.specifier().to_string(),
      range: d.range().map(|(text_info, range)| JsonLintDiagnosticRange {
        start: JsonDiagnosticLintPosition::new(
          range.start.as_byte_index(text_info.range().start),
          text_info.line_and_column_index(range.start),
        ),
        end: JsonDiagnosticLintPosition::new(
          range.end.as_byte_index(text_info.range().start),
          text_info.line_and_column_index(range.end),
        ),
      }),
      message: d.message().to_string(),
      code: d.code().to_string(),
      hint: d.hint().map(|h| h.to_string()),
    });
  }

  fn visit_error(&mut self, file_path: &str, err: &AnyError) {
    self.errors.push(LintError {
      file_path: file_path.to_string(),
      message: err.to_string(),
    });
  }

  fn close(&mut self, _check_count: usize) {
    sort_diagnostics(&mut self.diagnostics);
    let json = serde_json::to_string_pretty(&self);
    println!("{}", json.unwrap());
  }
}

fn sort_diagnostics(diagnostics: &mut [JsonLintDiagnostic]) {
  // Sort so that we guarantee a deterministic output which is useful for tests
  diagnostics.sort_by(|a, b| {
    use std::cmp::Ordering;
    let file_order = a.filename.cmp(&b.filename);
    match file_order {
      Ordering::Equal => match &a.range {
        Some(a_range) => match &b.range {
          Some(b_range) => {
            let line_order = a_range.start.line.cmp(&b_range.start.line);
            match line_order {
              Ordering::Equal => a_range.start.col.cmp(&b_range.start.col),
              _ => line_order,
            }
          }
          None => Ordering::Less,
        },
        None => match &b.range {
          Some(_) => Ordering::Greater,
          None => Ordering::Equal,
        },
      },
      _ => file_order,
    }
  });
}

fn get_config_rules_err_empty(
  rules: LintRulesConfig,
  maybe_config_file: Option<&deno_config::ConfigFile>,
) -> Result<ConfiguredRules, AnyError> {
  let lint_rules = get_configured_rules(rules, maybe_config_file);
  if lint_rules.rules.is_empty() {
    bail!("No rules have been configured")
  }
  Ok(lint_rules)
}

#[derive(Debug, Clone)]
pub struct ConfiguredRules {
  pub rules: Vec<&'static dyn LintRule>,
  // cli specific rules
  pub no_slow_types: bool,
}

impl ConfiguredRules {
  fn incremental_cache_state(&self) -> Vec<&str> {
    // use a hash of the rule names in order to bust the cache
    let mut names = self.rules.iter().map(|r| r.code()).collect::<Vec<_>>();
    // ensure this is stable by sorting it
    names.sort_unstable();
    if self.no_slow_types {
      names.push("no-slow-types");
    }
    names
  }
}

pub fn get_configured_rules(
  rules: LintRulesConfig,
  maybe_config_file: Option<&deno_config::ConfigFile>,
) -> ConfiguredRules {
  const NO_SLOW_TYPES_NAME: &str = "no-slow-types";
  let implicit_no_slow_types = maybe_config_file
    .map(|c| c.is_package() || !c.json.workspaces.is_empty())
    .unwrap_or(false);
  let no_slow_types = implicit_no_slow_types
    && !rules
      .exclude
      .as_ref()
      .map(|exclude| exclude.iter().any(|i| i == NO_SLOW_TYPES_NAME))
      .unwrap_or(false);
  let rules = rules::get_filtered_rules(
    rules
      .tags
      .or_else(|| Some(get_default_tags(maybe_config_file))),
    rules.exclude.map(|exclude| {
      exclude
        .into_iter()
        .filter(|c| c != NO_SLOW_TYPES_NAME)
        .collect()
    }),
    rules.include.map(|include| {
      include
        .into_iter()
        .filter(|c| c != NO_SLOW_TYPES_NAME)
        .collect()
    }),
  );
  ConfiguredRules {
    rules,
    no_slow_types,
  }
}

fn get_default_tags(
  maybe_config_file: Option<&deno_config::ConfigFile>,
) -> Vec<String> {
  let mut tags = Vec::with_capacity(2);
  tags.push("recommended".to_string());
  if maybe_config_file.map(|c| c.is_package()).unwrap_or(false) {
    tags.push("jsr".to_string());
  }
  tags
}

#[cfg(test)]
mod test {
  use deno_lint::rules::get_recommended_rules;

  use super::*;
  use crate::args::LintRulesConfig;

  #[test]
  fn recommended_rules_when_no_tags_in_config() {
    let rules_config = LintRulesConfig {
      exclude: Some(vec!["no-debugger".to_string()]),
      include: None,
      tags: None,
    };
    let rules = get_configured_rules(rules_config, None);
    let mut rule_names = rules
      .rules
      .into_iter()
      .map(|r| r.code().to_string())
      .collect::<Vec<_>>();
    rule_names.sort();
    let mut recommended_rule_names = get_recommended_rules()
      .into_iter()
      .map(|r| r.code().to_string())
      .filter(|n| n != "no-debugger")
      .collect::<Vec<_>>();
    recommended_rule_names.sort();
    assert_eq!(rule_names, recommended_rule_names);
  }
}
