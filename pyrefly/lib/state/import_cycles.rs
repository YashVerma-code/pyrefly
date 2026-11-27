/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The `import-cycle` diagnostic, for modules that import themselves through other modules.
//!
//! The analysis is a separate pass that runs once a transaction finishes a run. It reads only
//! import statements and resolves them with the transaction's import resolution, so it needs no
//! bindings or types and stays independent of how modules are bound and solved. Finding every
//! cycle means reading the imports of every module that a checked module reaches, which may mean
//! loading and parsing them again, so unlike other diagnostics, which are always computed and
//! then filtered by severity, the pass runs only when `import-cycle` is enabled for a checked
//! module.
//!
//! Imports are read from the source as written: every import statement counts, including
//! imports in functions, under `if TYPE_CHECKING:`, and in branches that never run. `import a.b`
//! imports `a` and `a.b`, and `from a import b` imports `a` and, if it is a module, `a.b`. Only
//! first-party source modules take part, so an import that resolves to a stub or to a module in
//! a site-package path ends the chain. An import of a file that is open in an editor reaches its
//! open contents, as checking the open file does.
//!
//! Each strongly connected component of the import graph gets one diagnostic, reported by the
//! component's module whose name sorts first among those that report `import-cycle`, at an
//! import that starts a shortest cycle through that module.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;

use dupe::Dupe;
use dupe::OptionDupedExt;
use pyrefly_build::handle::Handle;
use pyrefly_python::module_name::ModuleName;
use pyrefly_python::module_path::ModulePath;
use pyrefly_python::module_path::ModulePathDetails;
use pyrefly_util::tarjan::Tarjan;
use pyrefly_util::task_heap::Cancelled;
use pyrefly_util::thread_pool::ThreadPool;
use rayon::prelude::*;
use ruff_python_ast::Expr;
use ruff_python_ast::ModModule;
use ruff_python_ast::Stmt;
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::visitor::walk_stmt;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;

use crate::config::config::ConfigFile;
use crate::config::error_kind::ErrorKind;
use crate::error::collector::ErrorCollector;
use crate::error::collector::ModuleDiagnostics;
use crate::error::style::ErrorStyle;
use crate::module::module_info::ModuleInfo;
use crate::module::parse::module_parse;
use crate::state::load::Load;
use crate::state::state::Transaction;

/// A module named by an import statement, with the range at which to report importing it.
#[derive(Debug, Clone)]
struct ImportTarget {
    module: ModuleName,
    range: TextRange,
}

/// The import targets of a module's source, kept with the `Load` they were read from.
#[derive(Debug, Clone)]
struct ModuleImports {
    load: Arc<Load>,
    targets: Arc<[ImportTarget]>,
}

/// A module of the import graph, with the modules its imports resolve to.
struct Node {
    imports: ModuleImports,
    edges: Vec<(Handle, TextRange)>,
}

/// The result of the last import-cycle analysis.
#[derive(Debug, Clone, Default)]
pub struct ImportCycles {
    /// The import targets of each module that the analysis visited. A module whose `Load` is
    /// unchanged has unchanged source, so the next analysis reuses its targets.
    imports: Arc<HashMap<Handle, ModuleImports>>,
    /// The `import-cycle` diagnostics, stored on the module that reports each cycle.
    errors: Arc<HashMap<Handle, ModuleDiagnostics>>,
}

impl ImportCycles {
    pub fn errors(&self, handle: &Handle) -> Option<ModuleDiagnostics> {
        self.errors.get(handle).duped()
    }

    /// The analysis for `transaction` after a completed run, reusing the import targets of
    /// modules whose source has not changed since this analysis.
    ///
    /// Only finding the modules that report `import-cycle` scans the whole transaction. When
    /// there are none, as under the default configuration, the analysis does no graph work and
    /// keeps no results.
    pub fn update(&self, transaction: &Transaction, pool: &ThreadPool) -> Result<Self, Cancelled> {
        let reporters: HashSet<Handle> = transaction
            .handles()
            .into_iter()
            .filter(|handle| reports(transaction, handle))
            .collect();
        if reporters.is_empty() {
            return Ok(Self::default());
        }
        let cancellation = transaction.get_cancellation_handle();
        let check_cancelled = || {
            if cancellation.is_cancelled() {
                Err(Cancelled)
            } else {
                Ok(())
            }
        };
        let graph = self.graph(transaction, &reporters, pool, &check_cancelled)?;

        let mut tarjan = Tarjan::new();
        let visit = |handle: &Handle, f: &mut dyn FnMut(Handle)| {
            for (target, _) in &graph[handle].edges {
                f(target.dupe());
            }
        };
        let mut seen = HashSet::new();
        let mut errors = HashMap::new();
        for handle in graph.keys() {
            check_cancelled()?;
            let scc = tarjan.root(handle.dupe(), &visit);
            if !seen.insert(scc) {
                continue;
            }
            let members: HashSet<&Handle> = tarjan.iter_scc(scc).collect();
            if members.len() < 2 {
                continue;
            }
            let Some(owner) = members
                .iter()
                .copied()
                .filter(|member| reporters.contains(*member))
                .min_by(|a, b| {
                    (a.module().as_str(), a.path()).cmp(&(b.module().as_str(), b.path()))
                })
            else {
                continue;
            };
            let (range, cycle) = shortest_cycle(&graph, &members, owner);
            let load = &graph[owner].imports.load;
            let collector = ErrorCollector::new(load.module_info.dupe(), load.error_style);
            let chain = cycle
                .iter()
                .map(|handle| format!("`{}`", handle.module()))
                .collect::<Vec<_>>()
                .join(" -> ");
            collector
                .error_builder(
                    range,
                    ErrorKind::ImportCycle,
                    format!("Import cycle: {chain}"),
                )
                .emit();
            errors.insert(owner.dupe(), collector.finish());
        }
        Ok(Self {
            imports: Arc::new(
                graph
                    .into_iter()
                    .map(|(handle, node)| (handle, node.imports))
                    .collect(),
            ),
            errors: Arc::new(errors),
        })
    }

    /// The import graph of the source modules reachable from `reporters`. Each breadth-first
    /// layer is read in parallel, since reading a module's imports may parse it.
    fn graph(
        &self,
        transaction: &Transaction,
        reporters: &HashSet<Handle>,
        pool: &ThreadPool,
        check_cancelled: &(impl Fn() -> Result<(), Cancelled> + Sync),
    ) -> Result<HashMap<Handle, Node>, Cancelled> {
        let mut graph = HashMap::new();
        let mut queued: HashSet<Handle> = reporters.clone();
        let mut layer: Vec<Handle> = reporters.iter().cloned().collect();
        while !layer.is_empty() {
            let nodes: Vec<(Handle, Node)> = pool.install(|| {
                layer
                    .into_par_iter()
                    .map(|handle| {
                        check_cancelled()?;
                        let node = self.node(transaction, &handle);
                        Ok((handle, node))
                    })
                    .collect::<Result<_, _>>()
            })?;
            layer = Vec::new();
            for (handle, node) in nodes {
                for (target, _) in &node.edges {
                    if queued.insert(target.dupe()) {
                        layer.push(target.dupe());
                    }
                }
                graph.insert(handle, node);
            }
        }
        Ok(graph)
    }

    fn node(&self, transaction: &Transaction, handle: &Handle) -> Node {
        let load = transaction.get_or_load(handle);
        let imports = match self.imports.get(handle) {
            Some(imports) if Arc::ptr_eq(&imports.load, &load) => imports.clone(),
            _ => {
                let targets = match transaction.get_ast(handle) {
                    Some(ast) => import_targets(&ast, &load.module_info),
                    None => {
                        let (ast, _, _) = module_parse(
                            load.module_info.contents(),
                            handle.sys_info().version(),
                            load.module_info.source_type(),
                            &ErrorCollector::new(load.module_info.dupe(), ErrorStyle::Never),
                            false,
                        );
                        import_targets(&ast, &load.module_info)
                    }
                };
                ModuleImports { load, targets }
            }
        };
        let config = transaction
            .get_config(handle)
            .expect("a loaded module has a config");
        let edges = imports
            .targets
            .iter()
            .filter_map(|target| {
                let imported = transaction
                    .import_handle(handle, target.module, None)
                    .finding()?;
                if !is_first_party_source(&config, handle, &imported) {
                    return None;
                }
                // An open file is checked as an in-memory module, while imports resolve to the
                // file on disk, so an import of an open file reaches the in-memory module that this
                // transaction holds for it, if that module is in the importer's environment.
                let imported = match imported.path().details() {
                    ModulePathDetails::FileSystem(path) if transaction.has_memory(path) => {
                        let open = imported
                            .for_module(imported.module(), ModulePath::memory((**path).clone()))
                            .with_importer_site_package_path(imported.importer_site_package_path());
                        if transaction.get_load(&open).is_some() {
                            open
                        } else {
                            imported
                        }
                    }
                    _ => imported,
                };
                (imported != *handle).then_some((imported, target.range))
            })
            .collect();
        Node { imports, edges }
    }
}

/// Whether `handle`, imported by `importer` whose config is `config`, can be part of an import
/// cycle: first-party source, rather than a stub or a module installed in a site-package path of
/// the Python environment that `importer` is checked in.
fn is_first_party_source(config: &ConfigFile, importer: &Handle, handle: &Handle) -> bool {
    let path = handle.path();
    let environment = importer.importer_site_package_path();
    !path.is_interface()
        && !path.is_bundled()
        && !config.is_third_party_path(
            path.as_path(),
            environment.as_ref().map(|environment| environment.paths()),
        )
}

/// Whether `handle` is a checked first-party source module that reports `import-cycle`.
fn reports(transaction: &Transaction, handle: &Handle) -> bool {
    transaction
        .get_load(handle)
        .is_some_and(|load| load.error_style != ErrorStyle::Never)
        && transaction.get_config(handle).is_some_and(|config| {
            is_first_party_source(&config, handle, handle)
                && config
                    .errors(handle.path())
                    .severity(ErrorKind::ImportCycle)
                    .is_enabled()
        })
}

/// A shortest cycle from `owner` back to itself within the strongly connected component
/// `members`, along with the range of its first import. The cycle starts and ends with `owner`.
fn shortest_cycle(
    graph: &HashMap<Handle, Node>,
    members: &HashSet<&Handle>,
    owner: &Handle,
) -> (TextRange, Vec<Handle>) {
    // Each reached module maps to the module that first reached it, and the range of the import
    // that `owner` started the path with.
    let mut reached: HashMap<&Handle, (&Handle, TextRange)> = HashMap::new();
    let mut queue = VecDeque::from([owner]);
    while let Some(handle) = queue.pop_front() {
        let first = reached.get(handle).map(|(_, first)| *first);
        for (target, range) in &graph[handle].edges {
            let first_range = first.unwrap_or(*range);
            if target == owner {
                let mut cycle = vec![owner.dupe(), handle.dupe()];
                let mut current = handle;
                while let Some((previous, _)) = reached.get(current) {
                    cycle.push((*previous).dupe());
                    current = previous;
                }
                cycle.reverse();
                return (first_range, cycle);
            }
            if members.contains(target) && target != owner && !reached.contains_key(target) {
                reached.insert(target, (handle, first_range));
                queue.push_back(target);
            }
        }
    }
    unreachable!("a module in a strongly connected component of size 2 or more is on a cycle")
}

/// The modules that the import statements of `ast` name, in source order.
fn import_targets(ast: &ModModule, module_info: &ModuleInfo) -> Arc<[ImportTarget]> {
    struct ImportTargets<'a> {
        module_info: &'a ModuleInfo,
        targets: Vec<ImportTarget>,
    }

    impl<'a> Visitor<'a> for ImportTargets<'_> {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            match stmt {
                Stmt::Import(x) => {
                    for alias in &x.names {
                        let module = ModuleName::from_name(&alias.name.id);
                        let range = alias.name.range;
                        let name = module.as_str();
                        for (end, _) in name.match_indices('.') {
                            self.targets.push(ImportTarget {
                                module: ModuleName::from_str(&name[..end]),
                                range,
                            });
                        }
                        self.targets.push(ImportTarget { module, range });
                    }
                }
                Stmt::ImportFrom(x) => {
                    let Some(base) = self.module_info.name().new_maybe_relative(
                        self.module_info.path().is_init(),
                        x.level,
                        x.module.as_ref().map(|module| &module.id),
                    ) else {
                        return;
                    };
                    self.targets.push(ImportTarget {
                        module: base,
                        range: x.module.as_ref().map_or(x.range, |module| module.range()),
                    });
                    for alias in &x.names {
                        if &alias.name != "*" {
                            self.targets.push(ImportTarget {
                                module: base.append(&alias.name.id),
                                range: alias.name.range,
                            });
                        }
                    }
                }
                _ => walk_stmt(self, stmt),
            }
        }

        // Import statements never appear inside expressions.
        fn visit_expr(&mut self, _: &'a Expr) {}
    }

    let mut visitor = ImportTargets {
        module_info,
        targets: Vec::new(),
    };
    visitor.visit_body(&ast.body);
    visitor.targets.into()
}
