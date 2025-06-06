// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, BufWriter},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use json_comments::StripComments;
use lsp_types::{InlayHintKind, InlayHintLabel, InlayHintTooltip, Position};
use move_analyzer::{
    code_action::access_chain_autofix_actions_for_error,
    completions::{compute_completions_with_symbols, utils::compute_cursor},
    inlay_hints::inlay_hints_internal,
    symbols::{
        Symbols,
        compilation::{CompiledPkgInfo, SymbolsComputationData, get_compiled_pkg},
        compute_symbols, compute_symbols_parsed_program, compute_symbols_pre_process,
        requests::{def_info_doc_string, maybe_convert_for_guard},
        use_def::UseDefMap,
    },
};
use move_command_line_common::testing::insta_assert;
use move_compiler::linters::LintLevel;
use serde::{Deserialize, Serialize};
use url::Url;
use vfs::{MemoryFS, VfsPath};

//**************************************************************************************************
// Test Suites
//**************************************************************************************************

#[derive(Serialize, Deserialize)]
enum TestSuite {
    UseDef {
        project: String,
        file_tests: BTreeMap<String, Vec<UseDefTest>>,
    },
    AutoCompletion {
        project: String,
        file_tests: BTreeMap<String, Vec<AutoCompletionTest>>,
    },
    AutoImport {
        project: String,
        file_tests: BTreeMap<String, Vec<AutoImportTest>>,
    },
    Cursor {
        project: String,
        file_tests: BTreeMap<String, Vec<CursorTest>>,
    },
    Hint {
        project: String,
        file_tests: BTreeMap<String, Vec<HintTest>>,
    },
    AccessChainQuickFixTest {
        project: String,
        file_tests: BTreeMap<String, Vec<AccessChainQuickFixTest>>,
    },
}

#[derive(Serialize, Deserialize)]
struct UseDefTest {
    use_line: u32,
    use_ndx: usize,
}

#[derive(Serialize, Deserialize)]
struct AutoCompletionTest {
    use_line: u32,
    use_col: u32,
}

#[derive(Serialize, Deserialize)]
struct AutoImportTest {
    use_line: u32,
    use_col: u32,
}

#[derive(Serialize, Deserialize)]
struct CursorTest {
    line: u32,
    character: u32,
    description: String,
}

#[derive(Serialize, Deserialize)]
struct HintTest {
    use_line: u32,
    use_col: u32,
}

#[derive(Serialize, Deserialize)]
struct AccessChainQuickFixTest {
    err_line: u32,
    err_col: u32,
    err_msg: String,
}

//**************************************************************************************************
// Test Impls
//**************************************************************************************************
// These do the actual testing work.

impl UseDefTest {
    fn test(
        &self,
        test_idx: usize,
        mod_symbols: &UseDefMap,
        symbols: &Symbols,
        output: &mut dyn std::io::Write,
        use_file: &str,
        use_file_path: &Path,
    ) -> anyhow::Result<()> {
        let def_info = &symbols.def_info;
        let UseDefTest { use_ndx, use_line } = self;
        writeln!(output, "-- test {test_idx} -------------------")?;
        writeln!(output, "use line: {use_line}, use_ndx: {use_ndx}")?;
        let lsp_use_line = use_line - 1; // 0th-based
        let Some(uses) = mod_symbols.get(lsp_use_line) else {
            writeln!(
                output,
                "ERROR: No use_line {use_line} in mod_symbols {mod_symbols:#?} for file {use_file}"
            )?;
            return Ok(());
        };
        let Some(use_def) = uses.iter().nth(*use_ndx) else {
            writeln!(
                output,
                "ERROR: No use_line {use_ndx} in uses {uses:#?} for file {use_file}"
            )?;
            return Ok(());
        };
        let Some(mod_defs) = symbols.file_mods.get(use_file_path) else {
            writeln!(
                output,
                "ERROR: No modules found for file at {use_file_path:?}"
            )?;
            return Ok(());
        };
        // symbols.file_mods only has an entry if there are actual modules in the file
        // (BTreeSet containing module defs is never empty)
        debug_assert!(!mod_defs.is_empty());
        let use_file_hash = mod_defs.first().unwrap().fhash;
        let Some((_, use_file_content)) = symbols.files.get(&use_file_hash) else {
            writeln!(
                output,
                "ERROR: No use file content for file at {use_file_path:?}"
            )?;
            return Ok(());
        };
        let Some((_, def_file_content)) = symbols.files.get(&use_def.def_loc().file_hash()) else {
            writeln!(output, "ERROR: No def file content")?;
            return Ok(());
        };
        use_def.render(
            output,
            &symbols.files,
            lsp_use_line,
            &use_file_content,
            &def_file_content,
        )?;
        let Some(def) = def_info.get(&use_def.def_loc()) else {
            writeln!(output, "ERROR: No def loc found")?;
            return Ok(());
        };

        if let Some(guard_def) = maybe_convert_for_guard(
            def,
            use_file_path,
            &Position::new(lsp_use_line, use_def.col_start()),
            symbols,
        ) {
            writeln!(
                output,
                "On Hover:\n{}",
                if let Some(s) = def_info_doc_string(&guard_def) {
                    format!("{}\n\n{}", guard_def, s)
                } else {
                    format!("{}", guard_def)
                }
            )?;
        } else {
            writeln!(
                output,
                "On Hover:\n{}",
                if let Some(s) = def_info_doc_string(def) {
                    format!("{}\n\n{}", def, s)
                } else {
                    format!("{}", def)
                }
            )?;
        };
        Ok(())
    }
}

impl AutoCompletionTest {
    fn test(
        &self,
        test_idx: usize,
        compiled_pkg_info: &mut CompiledPkgInfo,
        symbols: &mut Symbols,
        output: &mut dyn std::io::Write,
        use_file_path: &Path,
    ) -> anyhow::Result<()> {
        completion_test(
            self.use_line,
            self.use_col,
            test_idx,
            compiled_pkg_info,
            symbols,
            output,
            use_file_path,
            false, // not for auto-import
        )
    }
}

impl AutoImportTest {
    fn test(
        &self,
        test_idx: usize,
        compiled_pkg_info: &mut CompiledPkgInfo,
        symbols: &mut Symbols,
        output: &mut dyn std::io::Write,
        use_file_path: &Path,
    ) -> anyhow::Result<()> {
        completion_test(
            self.use_line,
            self.use_col,
            test_idx,
            compiled_pkg_info,
            symbols,
            output,
            use_file_path,
            true, // for auto-import
        )
    }
}

impl CursorTest {
    fn test(
        &self,
        test_ndx: usize,
        mut compiled_pkg_info: CompiledPkgInfo,
        symbols: &mut Symbols,
        output: &mut dyn std::io::Write,
        path: &Path,
    ) -> anyhow::Result<()> {
        let CursorTest {
            line,
            character,
            description,
        } = self;
        let line = line - 1; // 0th-based
        let character = character - 1; // 0th-based

        // symbols do not change for each test, so we can reuse the same symbols
        // but we need to recompute the cursor each time
        let cursor_path = path.to_path_buf();
        let cursor_info = Some((&cursor_path, Position { line, character }));
        let mut symbols_computation_data = SymbolsComputationData::new();
        let mut symbols_computation_data_deps = SymbolsComputationData::new();
        let mut cursor_context = compute_symbols_pre_process(
            &mut symbols_computation_data,
            &mut symbols_computation_data_deps,
            &mut compiled_pkg_info,
            cursor_info,
        );
        cursor_context = compute_symbols_parsed_program(
            &mut symbols_computation_data,
            &mut symbols_computation_data_deps,
            &compiled_pkg_info,
            cursor_context,
        );
        symbols.cursor_context = cursor_context.clone();

        writeln!(
            output,
            "-- test {test_ndx} @ {line}:{character} ------------"
        )?;
        writeln!(output, "expected: {description}")?;
        writeln!(output, "{}", cursor_context.unwrap())?;
        Ok(())
    }
}

impl HintTest {
    fn test(
        &self,
        test_idx: usize,
        symbols: &Symbols,
        output: &mut dyn std::io::Write,
        use_file_path: &Path,
    ) -> anyhow::Result<()> {
        let inlay_hints = inlay_hints_internal(
            symbols,
            use_file_path.to_path_buf(),
            /* type_hints */ true,
            /* param_hints */ true,
        )
        .unwrap();
        let lsp_line = self.use_line - 1; // 0th-based
        let lsp_col = self.use_col - 1; // 0th-based

        writeln!(output, "-- test {test_idx} -------------------")?;
        let Some((hint, label_parts)) = inlay_hints.iter().find_map(|h| {
            if h.position.line == lsp_line && h.position.character == lsp_col {
                if let InlayHintLabel::LabelParts(parts) = &h.label {
                    return Some((h, parts));
                }
            }
            None
        }) else {
            writeln!(output, "NO INLAY HINT FOUND")?;
            return Ok(());
        };

        let tooltip = hint.tooltip.as_ref().map(|tip| match tip {
            InlayHintTooltip::String(s) => s.clone(),
            InlayHintTooltip::MarkupContent(m) => m.value.clone(),
        });

        match hint.kind {
            Some(InlayHintKind::TYPE) => {
                writeln!(output, "INLAY TYPE HINT : {}", label_parts[1].value)?
            }
            Some(InlayHintKind::PARAMETER) => {
                writeln!(output, "INLAY PARAM HINT: {}", label_parts[0].value)?
            }
            _ => writeln!(output, "INLAY HINT OF UNKNOWN TYPE")?,
        }
        if let Some(tip) = tooltip {
            writeln!(output, "ON HOVER:\n{}", tip)?;
        }

        Ok(())
    }
}

impl AccessChainQuickFixTest {
    fn test(
        &self,
        test_idx: usize,
        compiled_pkg_info: &mut CompiledPkgInfo,
        symbols: &mut Symbols,
        output: &mut dyn std::io::Write,
        use_file_path: &Path,
    ) -> anyhow::Result<()> {
        let err_line = self.err_line - 1; // 0th-based
        let err_col = self.err_col - 1; // 0th-based
        let err_pos = Position {
            line: err_line,
            character: err_col,
        };
        writeln!(output, "-- test {test_idx} -------------------")?;
        let mut code_actions = vec![];

        access_chain_autofix_actions_for_error(
            symbols,
            compiled_pkg_info,
            Url::from_file_path(use_file_path).unwrap(),
            err_pos,
            self.err_msg.clone(),
            None,
            &mut code_actions,
        );
        for action in code_actions {
            writeln!(output, "CODE ACTION: {}", action.title)?;
        }

        Ok(())
    }
}

fn completion_test(
    use_line: u32,
    use_col: u32,
    test_idx: usize,
    compiled_pkg_info: &mut CompiledPkgInfo,
    symbols: &mut Symbols,
    output: &mut dyn std::io::Write,
    use_file_path: &Path,
    auto_import: bool,
) -> anyhow::Result<()> {
    let lsp_use_line = use_line - 1; // 0th-based
    let lsp_use_col = use_col - 1; // 0th-based
    let use_pos = Position {
        line: lsp_use_line,
        character: lsp_use_col,
    };

    // symbols do not change for each test, so we can reuse the same symbols
    // but we need to recompute the cursor each time
    let cursor_path = use_file_path.to_path_buf();
    compute_cursor(symbols, compiled_pkg_info, &cursor_path, use_pos);

    let items = compute_completions_with_symbols(symbols, &cursor_path, use_pos, auto_import);
    writeln!(output, "-- test {test_idx} -------------------")?;
    writeln!(output, "use line: {}, use_col: {}", use_line, use_col)?;
    for i in items {
        writeln!(output, "{:?} '{}'", i.kind.unwrap(), i.label)?;
        if let Some(insert_text) = i.insert_text {
            writeln!(output, "    INSERT TEXT: '{}'", insert_text)?;
        }
        if let Some(label_details) = i.label_details {
            if let Some(detail) = label_details.detail {
                writeln!(output, "    TARGET     : '{}'", detail.trim())?;
            }
            if let Some(description) = label_details.description {
                writeln!(output, "    TYPE       : '{description}'")?;
            }
        }
        if let Some(additional_edit) = i.additional_text_edits {
            writeln!(
                output,
                "    ADDITIONAL EDIT: '{}'",
                additional_edit[0].new_text
            )?;
        }
    }
    writeln!(output)?;
    Ok(())
}

//**************************************************************************************************
// Test Suite Runner Code
//**************************************************************************************************

fn initial_symbols(
    project: String,
    files: &BTreeSet<&String>,
) -> datatest_stable::Result<(PathBuf, CompiledPkgInfo, Symbols)> {
    let base_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut project_path = base_path.clone();
    project_path.push(project);

    let ide_files_root: VfsPath = MemoryFS::new().into();
    let pkg_deps = Arc::new(Mutex::new(BTreeMap::new()));

    let (mut compiled_pkg_info_opt, _) = get_compiled_pkg(
        pkg_deps.clone(),
        ide_files_root.clone(),
        project_path.as_path(),
        None,
        LintLevel::None,
        BTreeMap::new(),
    )?;

    if let Some(f) = files.first() {
        let mod_file = project_path.join("sources").join(f);
        (compiled_pkg_info_opt, _) = get_compiled_pkg(
            pkg_deps.clone(),
            ide_files_root.clone(),
            project_path.as_path(),
            Some(vec![mod_file]),
            LintLevel::None,
            BTreeMap::new(),
        )?;
    }

    let compiled_pkg_info = compiled_pkg_info_opt.ok_or("PACKAGE COMPILATION FAILED")?;
    let symbols = compute_symbols(pkg_deps.clone(), compiled_pkg_info.clone(), None);

    Ok((project_path, compiled_pkg_info, symbols))
}

fn use_def_test_suite(
    project: String,
    file_tests: BTreeMap<String, Vec<UseDefTest>>,
) -> datatest_stable::Result<String> {
    let (project_path, _, symbols) = initial_symbols(project, &file_tests.keys().collect())?;

    let mut output: BufWriter<_> = BufWriter::new(Vec::new());
    let writer: &mut dyn io::Write = output.get_mut();

    for (file, tests) in file_tests {
        writeln!(
            writer,
            "== {file} ========================================================"
        )?;

        let mut fpath = project_path.clone();

        fpath.push(format!("sources/{file}"));
        let cpath = dunce::canonicalize(&fpath).unwrap();
        let mod_symbols = symbols
            .file_use_defs
            .get(&cpath)
            .ok_or(format!("NO SYMBOLS FOR {}", cpath.to_str().unwrap()))?;

        for (idx, test) in tests.iter().enumerate() {
            test.test(idx, mod_symbols, &symbols, writer, &file, &cpath)?;
            writeln!(writer)?;
        }
    }

    let result: String = String::from_utf8(output.into_inner().unwrap()).unwrap();
    Ok(result)
}

fn auto_completion_test_suite(
    project: String,
    file_tests: BTreeMap<String, Vec<AutoCompletionTest>>,
) -> datatest_stable::Result<String> {
    let (project_path, mut compiled_pkg_info, mut symbols) =
        initial_symbols(project, &file_tests.keys().collect())?;

    let mut output: BufWriter<_> = BufWriter::new(Vec::new());
    let writer: &mut dyn io::Write = output.get_mut();

    for (file, tests) in file_tests {
        writeln!(
            writer,
            "== {file} ========================================================"
        )?;

        let mut fpath = project_path.clone();

        fpath.push(format!("sources/{file}"));
        let cpath = dunce::canonicalize(&fpath).unwrap();

        for (idx, test) in tests.iter().enumerate() {
            test.test(idx, &mut compiled_pkg_info, &mut symbols, writer, &cpath)?;
        }
    }

    let result: String = String::from_utf8(output.into_inner().unwrap()).unwrap();
    Ok(result)
}

fn auto_import_test_suite(
    project: String,
    file_tests: BTreeMap<String, Vec<AutoImportTest>>,
) -> datatest_stable::Result<String> {
    let (project_path, mut compiled_pkg_info, mut symbols) =
        initial_symbols(project, &file_tests.keys().collect())?;

    let mut output: BufWriter<_> = BufWriter::new(Vec::new());
    let writer: &mut dyn io::Write = output.get_mut();

    for (file, tests) in file_tests {
        writeln!(
            writer,
            "== {file} ========================================================"
        )?;

        let mut fpath = project_path.clone();

        fpath.push(format!("sources/{file}"));
        let cpath = dunce::canonicalize(&fpath).unwrap();

        for (idx, test) in tests.iter().enumerate() {
            test.test(idx, &mut compiled_pkg_info, &mut symbols, writer, &cpath)?;
        }
    }

    let result: String = String::from_utf8(output.into_inner().unwrap()).unwrap();
    Ok(result)
}

fn cursor_test_suite(
    project: String,
    file_tests: BTreeMap<String, Vec<CursorTest>>,
) -> datatest_stable::Result<String> {
    let (project_path, compiled_pkg_info, mut symbols) =
        initial_symbols(project, &file_tests.keys().collect())?;

    let mut output: BufWriter<_> = BufWriter::new(Vec::new());
    let writer: &mut dyn io::Write = output.get_mut();

    for (file, tests) in file_tests {
        writeln!(
            writer,
            "== {file} ========================================================"
        )?;

        let mut fpath = project_path.clone();

        fpath.push(format!("sources/{file}"));
        let cpath = dunce::canonicalize(&fpath).unwrap();
        for (idx, test) in tests.iter().enumerate() {
            test.test(idx, compiled_pkg_info.clone(), &mut symbols, writer, &cpath)?;
        }
    }

    let result: String = String::from_utf8(output.into_inner().unwrap()).unwrap();
    Ok(result)
}

fn hint_test_suite(
    project: String,
    file_tests: BTreeMap<String, Vec<HintTest>>,
) -> datatest_stable::Result<String> {
    let (project_path, _, symbols) = initial_symbols(project, &file_tests.keys().collect())?;

    let mut output: BufWriter<_> = BufWriter::new(Vec::new());
    let writer: &mut dyn io::Write = output.get_mut();

    for (file, tests) in file_tests {
        writeln!(
            writer,
            "== {file} ========================================================"
        )?;

        let mut fpath = project_path.clone();

        fpath.push(format!("sources/{file}"));
        let cpath = dunce::canonicalize(&fpath).unwrap();

        for (idx, test) in tests.iter().enumerate() {
            test.test(idx, &symbols, writer, &cpath)?;
        }
    }

    let result: String = String::from_utf8(output.into_inner().unwrap()).unwrap();
    Ok(result)
}

fn access_chain_quick_fix_test_suite(
    project: String,
    file_tests: BTreeMap<String, Vec<AccessChainQuickFixTest>>,
) -> datatest_stable::Result<String> {
    let (project_path, mut compiled_pkg_info, mut symbols) =
        initial_symbols(project, &file_tests.keys().collect())?;

    let mut output: BufWriter<_> = BufWriter::new(Vec::new());
    let writer: &mut dyn io::Write = output.get_mut();

    for (file, tests) in file_tests {
        writeln!(
            writer,
            "== {file} ========================================================"
        )?;

        let mut fpath = project_path.clone();

        fpath.push(format!("sources/{file}"));
        let cpath = dunce::canonicalize(&fpath).unwrap();

        for (idx, test) in tests.iter().enumerate() {
            test.test(idx, &mut compiled_pkg_info, &mut symbols, writer, &cpath)?;
        }
    }

    let result: String = String::from_utf8(output.into_inner().unwrap()).unwrap();
    Ok(result)
}

fn move_ide_testsuite(test_path: &Path) -> datatest_stable::Result<()> {
    let suite_file = io::BufReader::new(File::open(test_path)?);
    let stripped = StripComments::new(suite_file);
    let suite: TestSuite = serde_json::from_reader(stripped)?;

    let output = match suite {
        TestSuite::UseDef {
            project,
            file_tests,
        } => use_def_test_suite(project, file_tests),
        TestSuite::AutoCompletion {
            project,
            file_tests,
        } => auto_completion_test_suite(project, file_tests),
        TestSuite::AutoImport {
            project,
            file_tests,
        } => auto_import_test_suite(project, file_tests),
        TestSuite::Cursor {
            project,
            file_tests,
        } => cursor_test_suite(project, file_tests),
        TestSuite::Hint {
            project,
            file_tests,
        } => hint_test_suite(project, file_tests),
        TestSuite::AccessChainQuickFixTest {
            project,
            file_tests,
        } => access_chain_quick_fix_test_suite(project, file_tests),
    }?;

    insta_assert! {
        input_path: test_path,
        contents: output,
    };
    Ok(())
}

datatest_stable::harness!(move_ide_testsuite, "tests/", r".*\.ide$");

/// Generates cursor tests as json -- useful for making a new batch of tests. Update this list,
/// set `harness = true` for this file in `Cargo.toml`,
/// and run `cargo nextest run generate_cursor_tests`.
#[allow(unused)]
#[test]
fn generate_cursor_test() {
    let posn_pairs = vec![
        (5, 23, "at a struct definition name"),
        (6, 13, "at a struct field name"),
        (6, 19, "in a struct definition at nowhere in particular"),
        (6, 20, "at a struct field type"),
        (6, 21, "at a struct field type"),
        (6, 22, "at a struct field type"),
        (6, 23, "in a struct definition at nowhere in particular"),
        (17, 45, "at a function return type"),
        (28, 14, "at a function definition at nowhere in particular"),
        (28, 14, "at a function definition name"),
        (28, 15, "at a function definition name"),
        (28, 16, "at a function definition name"),
        (28, 17, "at a function definition name"),
        (28, 18, "at a function definition at nowhere in particular"),
        (28, 19, "at a function definition at nowhere in particular"),
        (31, 5, "inside a function body -- unknown"),
        (31, 9, "let binding"),
        (31, 19, "binding in the lhs of a let-binding"),
        (31, 50, "name in module access"),
        (33, 29, "name in a dot call"),
        (33, 34, "equal sign in a binop"),
    ];

    let tests = posn_pairs
        .into_iter()
        .map(|(line, character, description)| CursorTest {
            line,
            character,
            description: description.to_string(),
        })
        .collect::<Vec<_>>();
    let test = TestSuite::Cursor {
        project: "tests/move-2024".to_string(),
        file_tests: BTreeMap::from([("dot_call.move".to_string(), tests)]),
    };

    let string = format!("{}\n", serde_json::to_string_pretty(&test).unwrap());
    let _ = fs::write("cursor_dot_call_tests.ide", string);
}
