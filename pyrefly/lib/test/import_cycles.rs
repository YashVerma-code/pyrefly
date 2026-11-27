/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Tests of the `import-cycle` diagnostic. See https://github.com/pyrefly-org/pyrefly/issues/3676.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use dupe::Dupe;
use pyrefly_build::handle::Handle;
use pyrefly_python::module_name::ModuleName;
use pyrefly_python::module_path::ModulePath;
use pyrefly_python::sys_info::SysInfo;
use pyrefly_util::arc_id::ArcId;
use pyrefly_util::thread_pool::TEST_THREAD_COUNT;

use crate::config::config::ConfigFile;
use crate::config::error::ErrorDisplayConfig;
use crate::config::error_kind::ErrorKind;
use crate::config::error_kind::Severity;
use crate::config::finder::ConfigFinder;
use crate::state::load::FileContents;
use crate::state::require::Require;
use crate::state::state::State;
use crate::state::state::Transaction;
use crate::state::subscriber::TestSubscriber;
use crate::test::util::TestEnv;
use crate::testcase;

fn env() -> TestEnv {
    TestEnv::new().enable_import_cycle_error()
}

fn env_with(modules: &[(&str, &str, &str)]) -> TestEnv {
    let mut env = env();
    for (name, path, code) in modules {
        env.add_with_path(name, path, code);
    }
    env
}

testcase!(
    test_two_module_cycle,
    env_with(&[("x", "x.py", "import main\n")]),
    r#"
import x  # E: Import cycle: `main` -> `x` -> `main`
"#,
);

testcase!(
    test_three_module_cycle,
    env_with(&[
        ("x", "x.py", "from y import g\ndef f() -> None: ...\n"),
        ("y", "y.py", "from main import h\ndef g() -> None: ...\n"),
    ]),
    r#"
from x import f  # E: Import cycle: `main` -> `x` -> `y` -> `main`
def h() -> None: ...
"#,
);

testcase!(
    test_type_checking_cycle,
    env_with(&[(
        "x",
        "x.py",
        "from typing import TYPE_CHECKING\nif TYPE_CHECKING:\n    from main import A\nclass B: ...\n",
    )]),
    r#"
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    from x import B  # E: Import cycle: `main` -> `x` -> `main`
class A: ...
"#,
);

testcase!(
    test_function_local_import_cycle,
    env_with(&[("x", "x.py", "def f() -> None:\n    import main\n")]),
    r#"
import x  # E: Import cycle: `main` -> `x` -> `main`
"#,
);

testcase!(
    test_no_cycle,
    env_with(&[("x", "x.py", "import y\n"), ("y", "y.py", "")]),
    r#"
import main
import x
"#,
);

testcase!(
    test_stub_is_not_followed,
    env_with(&[("x", "x.pyi", "import main\n")]),
    r#"
import x
"#,
);

// A module in a site-package path is installed rather than first-party.
testcase!(
    test_site_package_is_not_followed,
    {
        let mut env = env().with_site_package_paths(vec![PathBuf::from("deps")]);
        env.add_with_path("lib", "deps/lib.py", "import main\n");
        env
    },
    r#"
import lib
"#,
);

// Only a site-package path makes a module installed, not a directory name.
testcase!(
    test_directory_named_site_packages_is_first_party,
    env_with(&[("x", "site-packages/x.py", "import main\n")]),
    r#"
import x  # E: Import cycle: `main` -> `x` -> `main`
"#,
);

testcase!(
    test_disabled_by_default,
    {
        let mut env = TestEnv::new();
        env.add("x", "import main\n");
        env
    },
    r#"
import x
"#,
);

// A strongly connected component with several cycles gets one diagnostic.
testcase!(
    test_one_error_per_component,
    env_with(&[
        ("x", "x.py", "import main\n"),
        ("y", "y.py", "import main\n")
    ]),
    r#"
import x  # E: Import cycle: `main` -> `x` -> `main`
import y
"#,
);

// The cycle is reported in `a`, the module of the cycle whose name sorts first.
testcase!(
    test_cycle_reported_in_first_module,
    env_with(&[(
        "a",
        "a.py",
        "import main  # E: Import cycle: `a` -> `main` -> `a`\n",
    )]),
    r#"
import a
"#,
);

// Importing `pkg.sub` also imports the package `pkg`.
testcase!(
    test_import_of_submodule_imports_package,
    env_with(&[
        ("pkg", "pkg/__init__.py", "import main\n"),
        ("pkg.sub", "pkg/sub.py", ""),
    ]),
    r#"
import pkg.sub  # E: Import cycle: `main` -> `pkg` -> `main`
"#,
);

testcase!(
    test_from_import_of_submodule,
    env_with(&[
        ("pkg", "pkg/__init__.py", ""),
        ("pkg.child", "pkg/child.py", "import main\n"),
    ]),
    r#"
from pkg import child  # E: Import cycle: `main` -> `pkg.child` -> `main`
"#,
);

// `from . import sibling` imports the package itself, which imports `pkg.mod`.
testcase!(
    test_relative_import_of_package,
    env_with(&[
        (
            "pkg",
            "pkg/__init__.py",
            "from . import mod  # E: Import cycle: `pkg` -> `pkg.mod` -> `pkg`\n",
        ),
        ("pkg.mod", "pkg/mod.py", "from . import sibling\n"),
        ("pkg.sibling", "pkg/sibling.py", ""),
    ]),
    r#"
import pkg
"#,
);

// `from pkg.other import x` imports `pkg.other`, but `pkg` is not counted as imported.
testcase!(
    test_from_import_does_not_import_package,
    env_with(&[
        ("pkg", "pkg/__init__.py", "from . import mod\n"),
        ("pkg.mod", "pkg/mod.py", "from pkg.other import x\n"),
        ("pkg.other", "pkg/other.py", "x = 1\n"),
    ]),
    r#"
import pkg
"#,
);

testcase!(
    test_suppressed_cycle,
    env_with(&[("x", "x.py", "import main\n")]),
    r#"
import x  # pyrefly: ignore[import-cycle]
"#,
);

/// A project of the modules in `env`, where the `import-cycle` error is enabled in the
/// modules for which `enabled` returns true.
fn project(env: &TestEnv, enabled: impl Fn(&Path) -> bool + Send + Sync + 'static) -> State {
    let on = env.clone().enable_import_cycle_error().config();
    let off = env.config();
    let finder = ConfigFinder::new_lookup(
        Arc::new(
            move |path| {
                if enabled(path) { on.dupe() } else { off.dupe() }
            },
        ),
        Box::new(|config, _| config),
    );
    State::new(finder, TEST_THREAD_COUNT)
}

fn handle(env: &TestEnv, name: &str) -> Handle {
    Handle::new(
        ModuleName::from_str(name),
        ModulePath::memory(PathBuf::from(format!("{name}.py"))),
        env.sys_info(),
    )
}

/// The `import-cycle` diagnostics of `handles`, as `module:range: message`.
fn import_cycle_errors(transaction: &Transaction, handles: &[Handle]) -> Vec<String> {
    transaction
        .get_errors(handles)
        .collect_errors()
        .ordinary
        .iter()
        .filter(|e| e.error_kind() == ErrorKind::ImportCycle)
        .map(|e| format!("{}:{}: {}", e.module().name(), e.display_range(), e.msg()))
        .collect()
}

#[test]
fn test_cycle_through_disabled_modules() {
    let mut env = TestEnv::new();
    env.add("a", "import b\n");
    env.add("b", "import c\n");
    env.add("c", "import a\n");
    let state = project(&env, |path| path == Path::new("c.py"));
    let handles = ["a", "b", "c"].map(|name| handle(&env, name));
    let mut transaction = state.new_one_shot_transaction(Require::Exports);
    transaction.set_memory(env.get_memory());
    transaction.run(&handles, Require::Errors, None);
    assert_eq!(
        import_cycle_errors(&transaction, &handles),
        ["c:1:8-9: Import cycle: `c` -> `a` -> `b` -> `c`"]
    );
}

#[test]
fn test_edit_creates_and_breaks_cycle() {
    let mut env = TestEnv::new();
    env.add("a", "import b\n");
    env.add("b", "def f() -> None:\n    pass\n");
    let state = project(&env, |_| true);
    let handles = ["a", "b"].map(|name| handle(&env, name));
    let run = |memory| {
        let mut transaction = state.new_committable_transaction(Require::Exports, None);
        transaction.as_mut().set_memory(memory);
        state.run_with_committing_transaction(
            transaction,
            &handles,
            Require::Everything,
            None,
            None,
        );
        import_cycle_errors(&state.transaction(), &handles)
    };
    let edit_b = |b: &str| {
        run(vec![(
            PathBuf::from("b.py"),
            Some(Arc::new(FileContents::from_source(b.to_owned()))),
        )])
    };
    assert_eq!(run(env.get_memory()), Vec::<String>::new());
    // Moving an import into a function body changes no export of `b`, so the cycle starts and
    // stops being reported without any change to the types that `a` reads.
    assert_eq!(
        edit_b("def f() -> None:\n    import a\n"),
        ["a:1:8-9: Import cycle: `a` -> `b` -> `a`"]
    );
    assert_eq!(edit_b("def f() -> None:\n    pass\n"), Vec::<String>::new());
}

#[test]
fn test_cycle_reported_by_module_checked_as_dependency() {
    let mut env = TestEnv::new();
    env.add("a", "");
    env.add("b", "from a import *\n");
    let state = project(&env, |path| path == Path::new("a.py"));
    let handles = ["a", "b"].map(|name| handle(&env, name));
    // Only `b`, where `import-cycle` is disabled, is checked directly, but the modules it imports
    // are checked too, so `a` reports `import-cycle`.
    let run = |memory| {
        let mut transaction = state.new_committable_transaction(Require::Errors, None);
        transaction.as_mut().set_memory(memory);
        state.run_with_committing_transaction(
            transaction,
            &handles[1..],
            Require::Errors,
            None,
            None,
        );
        import_cycle_errors(&state.transaction(), &handles)
    };
    assert_eq!(run(env.get_memory()), Vec::<String>::new());
    assert_eq!(
        run(vec![(
            PathBuf::from("a.py"),
            Some(Arc::new(FileContents::from_source("import b\n".to_owned()))),
        )]),
        ["a:1:8-9: Import cycle: `a` -> `b` -> `a`"]
    );
}

#[test]
fn test_cycle_through_open_file() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a.py");
    let b = root.path().join("b.py");
    fs::write(&a, "").unwrap();
    fs::write(&b, "import a\n").unwrap();
    let mut env = TestEnv::new();
    env.add_real_path("a", a.clone());
    env.add_real_path("b", b);
    let state = project(&env, |_| true);
    // An editor has `a.py` open with unsaved contents, so it is checked as an in-memory module,
    // while `b` imports `a` from disk.
    let open = Handle::new(
        ModuleName::from_str("a"),
        ModulePath::memory(a.clone()),
        env.sys_info(),
    );
    let mut transaction = state.new_committable_transaction(Require::Exports, None);
    transaction.as_mut().set_memory(vec![(
        a,
        Some(Arc::new(FileContents::from_source("import b\n".to_owned()))),
    )]);
    state.run_with_committing_transaction(
        transaction,
        &[open.dupe()],
        Require::Everything,
        None,
        None,
    );
    assert_eq!(
        import_cycle_errors(&state.transaction(), &[open]),
        ["a:1:8-9: Import cycle: `a` -> `b` -> `a`"]
    );
}

#[test]
fn test_module_read_only_for_imports_is_not_reported_to_subscriber() {
    let mut env = TestEnv::new();
    env.add(
        "a",
        "import sys\nif sys.version_info < (3, 0):\n    import b\n",
    );
    env.add("b", "import a\n");
    let state = project(&env, |path| path == Path::new("a.py"));
    let subscriber = TestSubscriber::new();
    let mut transaction =
        state.new_committable_transaction(Require::Exports, Some(Box::new(subscriber.dupe())));
    transaction.as_mut().set_memory(env.get_memory());
    state.run_with_committing_transaction(
        transaction,
        &[handle(&env, "a")],
        Require::Errors,
        None,
        None,
    );
    // Checking `a` skips the inactive import of `b`, so only reading the imports of `a` loads
    // `b`, and the subscriber hears only of the modules that are checked.
    assert_eq!(
        import_cycle_errors(&state.transaction(), &[handle(&env, "a")]),
        ["a:3:12-13: Import cycle: `a` -> `b` -> `a`"]
    );
    let b = handle(&env, "b");
    assert!(
        subscriber
            .finish()
            .get(&b)
            .is_none_or(|(starts, _)| *starts == 0)
    );
}

// `shared` holds first-party code: it is a site-package path of `a`'s config, but also on its
// search path. Its modules are checked in `a`'s Python environment, since their own config has
// another site-package path, while `deps` holds an installed package of that environment, so the
// cycles through `lib` are not reported.
#[test]
fn test_cycle_through_search_path_in_site_packages() {
    let root = tempfile::tempdir().unwrap();
    let [project, shared, deps] = ["project", "shared", "deps"].map(|dir| root.path().join(dir));
    for (dir, file, code) in [
        (&project, "a.py", "import lib\nimport b\n"),
        (&shared, "b.py", "import lib\nimport c\n"),
        (&shared, "c.py", "import a\n"),
        (&deps, "lib.py", "import a\n"),
    ] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(file), code).unwrap();
    }
    let config = |site_package_path: Vec<PathBuf>| {
        let mut config = ConfigFile {
            search_path_from_args: vec![project.clone(), shared.clone()],
            ..Default::default()
        };
        config.python_environment.site_package_path = Some(site_package_path);
        config.interpreters.skip_interpreter_query = true;
        config
            .root
            .errors
            .get_or_insert_with(ErrorDisplayConfig::default)
            .set_error_severity(ErrorKind::ImportCycle, Severity::Error);
        config.configure();
        ArcId::new(config)
    };
    let project_config = config(vec![shared.clone(), deps.clone()]);
    let shared_config = config(Vec::new());
    let shared_root = shared.clone();
    let finder = ConfigFinder::new_lookup(
        Arc::new(move |path| {
            if path.starts_with(&shared_root) {
                shared_config.dupe()
            } else {
                project_config.dupe()
            }
        }),
        Box::new(|config, _| config),
    );
    let state = State::new(finder, TEST_THREAD_COUNT);
    let a = Handle::new(
        ModuleName::from_str("a"),
        ModulePath::filesystem(project.join("a.py")),
        SysInfo::default(),
    );
    // The editor checks `b` in its own environment. Its unsaved contents must not replace
    // the separate copy of `b` checked in `a`'s environment.
    let open_b = Handle::new(
        ModuleName::from_str("b"),
        ModulePath::memory(shared.join("b.py")),
        SysInfo::default(),
    );
    let mut transaction = state.new_one_shot_transaction(Require::Exports);
    transaction.set_memory(vec![(
        shared.join("b.py"),
        Some(Arc::new(FileContents::from_source(String::new()))),
    )]);
    transaction.run(&[a.dupe(), open_b], Require::Errors, None);
    assert_eq!(
        import_cycle_errors(&transaction, &[a]),
        ["a:2:8-9: Import cycle: `a` -> `b` -> `c` -> `a`"]
    );
}

#[test]
fn test_config_change_enables_and_disables_cycle() {
    let mut env = TestEnv::new();
    env.add("a", "import b\n");
    env.add("b", "import a\n");
    let enabled = Arc::new(AtomicBool::new(false));
    let state = project(&env, {
        let enabled = enabled.dupe();
        move |_| enabled.load(Ordering::SeqCst)
    });
    let handles = ["a", "b"].map(|name| handle(&env, name));
    let mut transaction = state.new_committable_transaction(Require::Exports, None);
    transaction.as_mut().set_memory(env.get_memory());
    state.run_with_committing_transaction(transaction, &handles, Require::Everything, None, None);
    assert_eq!(
        import_cycle_errors(&state.transaction(), &handles),
        Vec::<String>::new()
    );

    let set_enabled = |value| {
        enabled.store(value, Ordering::SeqCst);
        let mut transaction = state.new_committable_transaction(Require::Exports, None);
        transaction.as_mut().invalidate_config();
        state.run_with_committing_transaction(
            transaction,
            &handles,
            Require::Everything,
            None,
            None,
        );
        import_cycle_errors(&state.transaction(), &handles)
    };
    assert_eq!(
        set_enabled(true),
        ["a:1:8-9: Import cycle: `a` -> `b` -> `a`"]
    );
    assert_eq!(set_enabled(false), Vec::<String>::new());
}
