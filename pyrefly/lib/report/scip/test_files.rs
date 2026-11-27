/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The files that pytest treats as test code, following its `python_files` setting.

use std::path::Path;

use anyhow::Context as _;
use anyhow::anyhow;
use configparser::ini::Ini;
use glob::MatchOptions;
use glob::Pattern;
use pyrefly_util::fs_anyhow;
use pyrefly_util::unix_path::path_to_unix_string;

/// The file that pytest loads fixtures and hooks from, in every directory it collects.
const CONFTEST: &str = "conftest.py";

/// How a file that pytest reads its configuration from stores its settings.
#[derive(Clone, Copy)]
enum ConfigFormat {
    /// `pytest.toml` or `.pytest.toml`, with settings in the `[pytest]` table.
    PytestToml,
    /// `pytest.ini` or `.pytest.ini`, with settings in the `[pytest]` section.
    PytestIni,
    /// `pyproject.toml`, with settings in `[tool.pytest]`, or in `[tool.pytest.ini_options]`
    /// with INI-style values.
    Pyproject,
    /// An INI file shared with other tools, with settings in the named section.
    SharedIni(&'static str),
}

/// The files pytest looks for its configuration in, in the order it looks for them in a
/// directory. It uses the first that holds its configuration: a file dedicated to pytest always
/// does, even when it is empty, and a shared file does when it has pytest's table or section.
const CONFIG_FILES: [(&str, ConfigFormat); 7] = [
    ("pytest.toml", ConfigFormat::PytestToml),
    (".pytest.toml", ConfigFormat::PytestToml),
    ("pytest.ini", ConfigFormat::PytestIni),
    (".pytest.ini", ConfigFormat::PytestIni),
    ("pyproject.toml", ConfigFormat::Pyproject),
    ("tox.ini", ConfigFormat::SharedIni("pytest")),
    ("setup.cfg", ConfigFormat::SharedIni("tool:pytest")),
];

/// Matches like Python's `fnmatch`, which pytest uses: `*` also matches path separators, and
/// case is ignored only on Windows.
const MATCH_OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: !cfg!(windows),
    require_literal_separator: false,
    require_literal_leading_dot: false,
};

/// A `python_files` pattern, matched as pytest matches it.
enum FilePattern {
    /// A pattern without a `/`, matched against the file name.
    Name(Pattern),
    /// A pattern with a `/`, matched against the whole path. A relative pattern matches at any
    /// depth.
    Path(Pattern),
}

/// The `.py` files that pytest collects tests from, as its `python_files` setting names them,
/// and the `conftest.py` files it loads fixtures and hooks from.
pub struct TestFiles {
    patterns: Vec<FilePattern>,
}

impl TestFiles {
    /// Reads `python_files` from the file in `root` that pytest takes its configuration from
    /// when it runs in `root`. A configuration in a parent directory is not read.
    pub fn project(root: &Path) -> anyhow::Result<Self> {
        for (name, format) in CONFIG_FILES {
            let path = root.join(name);
            if !path.is_file() {
                continue;
            }
            let python_files = python_files(&fs_anyhow::read_to_string(&path)?, format)
                .with_context(|| {
                    format!(
                        "while reading pytest's configuration in `{}`",
                        path.display()
                    )
                })?;
            if let Some(python_files) = python_files {
                return Self::new(&python_files);
            }
        }
        Self::new(&default_python_files())
    }

    fn new(python_files: &[String]) -> anyhow::Result<Self> {
        let patterns = python_files
            .iter()
            .map(|pattern| {
                let file_pattern = if !pattern.contains('/') {
                    Pattern::new(pattern).map(FilePattern::Name)
                } else if pattern.starts_with('/') {
                    Pattern::new(pattern).map(FilePattern::Path)
                } else {
                    Pattern::new(&format!("*/{pattern}")).map(FilePattern::Path)
                };
                file_pattern
                    .with_context(|| format!("unsupported `python_files` pattern `{pattern}`"))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self { patterns })
    }

    /// Whether `path` is a `.py` file that pytest collects tests from, or a `conftest.py`.
    pub fn contains(&self, path: &Path) -> bool {
        if path.extension().is_none_or(|extension| extension != "py") {
            return false;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        name == CONFTEST
            || self.patterns.iter().any(|pattern| match pattern {
                FilePattern::Name(pattern) => pattern.matches_with(name, MATCH_OPTIONS),
                FilePattern::Path(pattern) => {
                    pattern.matches_with(&path_to_unix_string(path), MATCH_OPTIONS)
                }
            })
    }
}

/// The patterns pytest collects test modules from when its configuration sets none.
fn default_python_files() -> Vec<String> {
    vec!["test_*.py".to_owned(), "*_test.py".to_owned()]
}

/// The `python_files` patterns in a file that pytest looks for its configuration in: `None` if
/// the file doesn't hold pytest's configuration, and pytest's defaults if it does but sets no
/// patterns.
fn python_files(contents: &str, format: ConfigFormat) -> anyhow::Result<Option<Vec<String>>> {
    match format {
        ConfigFormat::PytestToml => {
            let document: toml::Table = contents.parse()?;
            match document.get("pytest").and_then(toml::Value::as_table) {
                Some(settings) => toml_python_files(settings).map(Some),
                None => Ok(Some(default_python_files())),
            }
        }
        ConfigFormat::Pyproject => {
            let document: toml::Table = contents.parse()?;
            let Some(pytest) = document
                .get("tool")
                .and_then(|tool| tool.get("pytest"))
                .and_then(toml::Value::as_table)
                .filter(|pytest| !pytest.is_empty())
            else {
                return Ok(None);
            };
            let settings = pytest
                .get("ini_options")
                .and_then(toml::Value::as_table)
                .unwrap_or(pytest);
            toml_python_files(settings).map(Some)
        }
        ConfigFormat::PytestIni => Ok(Some(
            ini_python_files(contents, "pytest")?.unwrap_or_else(default_python_files),
        )),
        ConfigFormat::SharedIni(section) => ini_python_files(contents, section),
    }
}

/// The `python_files` patterns in a table of pytest settings: a list of patterns or, in
/// INI-style settings, a string of them.
fn toml_python_files(settings: &toml::Table) -> anyhow::Result<Vec<String>> {
    match settings.get("python_files") {
        None => Ok(default_python_files()),
        Some(toml::Value::String(patterns)) => Ok(split_patterns(patterns)),
        Some(toml::Value::Array(patterns)) => patterns
            .iter()
            .map(|pattern| {
                pattern
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("`python_files` pattern `{pattern}` is not a string"))
            })
            .collect(),
        Some(value) => Err(anyhow!(
            "`python_files` must be a list of patterns, not `{value}`"
        )),
    }
}

/// The `python_files` patterns in the `section` of an INI file, or `None` if it has no such
/// section. A value can continue on indented lines.
fn ini_python_files(contents: &str, section: &str) -> anyhow::Result<Option<Vec<String>>> {
    let mut ini = Ini::new_cs();
    ini.set_multiline(true);
    ini.read(contents.to_owned()).map_err(|e| anyhow!(e))?;
    if !ini.sections().iter().any(|name| name == section) {
        return Ok(None);
    }
    Ok(Some(
        ini.get(section, "python_files")
            .map_or_else(default_python_files, |patterns| split_patterns(&patterns)),
    ))
}

/// pytest splits a string of patterns like a shell command line. Patterns don't contain
/// whitespace or quotes in practice, so splitting on whitespace gives the same patterns.
fn split_patterns(patterns: &str) -> Vec<String> {
    patterns.split_whitespace().map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Which of some candidate files are test files in a project with these configuration files.
    fn test_files(config: &[(&str, &str)]) -> Vec<&'static str> {
        let root = tempfile::tempdir().unwrap();
        for (name, contents) in config {
            fs_anyhow::write(&root.path().join(name), contents).unwrap();
        }
        let test_files = TestFiles::project(root.path()).unwrap();
        [
            "test_app.py",
            "app_test.py",
            "check_app.py",
            "tests.py",
            "conftest.py",
            "test_app.pyi",
            "app.py",
            "suite/check_app.py",
        ]
        .into_iter()
        .filter(|path| test_files.contains(&root.path().join(path)))
        .collect()
    }

    #[test]
    fn test_default_patterns() {
        let expected = ["test_app.py", "app_test.py", "conftest.py"];
        assert_eq!(test_files(&[]), expected);
        // A `pyproject.toml` without pytest settings, and a `tox.ini` without a `[pytest]`
        // section, don't hold pytest's configuration.
        let unrelated = [
            ("pyproject.toml", "[project]\nname = \"app\"\n"),
            ("tox.ini", "[tox]\nenvlist = py313\n"),
        ];
        assert_eq!(test_files(&unrelated), expected);
    }

    #[test]
    fn test_ini_files() {
        let multiline = "[pytest]\npython_files =\n    check_*.py\n    tests.py\n";
        assert_eq!(
            test_files(&[("pytest.ini", multiline)]),
            [
                "check_app.py",
                "tests.py",
                "conftest.py",
                "suite/check_app.py"
            ]
        );
        let setup_cfg = "[tool:pytest]\npython_files = tests.py\n";
        assert_eq!(
            test_files(&[("setup.cfg", setup_cfg)]),
            ["tests.py", "conftest.py"]
        );
        // An empty `pytest.ini` is pytest's configuration, so the defaults apply.
        let tox_ini = "[pytest]\npython_files = tests.py\n";
        assert_eq!(
            test_files(&[("pytest.ini", ""), ("tox.ini", tox_ini)]),
            ["test_app.py", "app_test.py", "conftest.py"]
        );
    }

    #[test]
    fn test_toml_files() {
        let ini_options = "[tool.pytest.ini_options]\npython_files = \"tests.py check_*.py\"\n";
        assert_eq!(
            test_files(&[("pyproject.toml", ini_options)]),
            [
                "check_app.py",
                "tests.py",
                "conftest.py",
                "suite/check_app.py"
            ]
        );
        let native = "[tool.pytest]\npython_files = [\"tests.py\"]\n";
        assert_eq!(
            test_files(&[("pyproject.toml", native)]),
            ["tests.py", "conftest.py"]
        );
        // `pytest.toml` comes first, and a pattern with a `/` matches at any depth.
        let pytest_toml = "[pytest]\npython_files = [\"suite/check_*.py\"]\n";
        assert_eq!(
            test_files(&[("pytest.toml", pytest_toml), ("pyproject.toml", native)]),
            ["conftest.py", "suite/check_app.py"]
        );
    }

    #[test]
    fn test_invalid_configuration() {
        let root = tempfile::tempdir().unwrap();
        let pyproject = "[tool.pytest]\npython_files = 1\n";
        fs_anyhow::write(&root.path().join("pyproject.toml"), pyproject).unwrap();
        let error = TestFiles::project(root.path()).err().unwrap();
        assert!(format!("{error:#}").contains("must be a list of patterns"));
    }
}
