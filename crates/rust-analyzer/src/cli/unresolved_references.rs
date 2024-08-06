use hir::{db::HirDatabase, AnyDiagnostic, Crate, HirFileIdExt as _, Module, Semantics};
use ide::{AnalysisHost, RootDatabase, TextRange};
use ide_db::{
    base_db::SourceDatabaseExt as _, defs::NameRefClass, EditionedFileId, FxHashSet,
    LineIndexDatabase as _,
};
use load_cargo::{load_workspace_at, LoadCargoConfig, ProcMacroServerChoice};
use project_model::{CargoConfig, RustLibSource};
use syntax::{ast, AstNode, WalkEvent};
use vfs::FileId;

use crate::cli::flags;

impl flags::UnresolvedReferences {
    pub fn run(self) -> anyhow::Result<()> {
        let cargo_config =
            CargoConfig { sysroot: Some(RustLibSource::Discover), ..Default::default() };
        let with_proc_macro_server = if let Some(p) = &self.proc_macro_srv {
            let path = vfs::AbsPathBuf::assert_utf8(std::env::current_dir()?.join(p));
            ProcMacroServerChoice::Explicit(path)
        } else {
            ProcMacroServerChoice::Sysroot
        };
        let load_cargo_config = LoadCargoConfig {
            load_out_dirs_from_check: !self.disable_build_scripts,
            with_proc_macro_server,
            prefill_caches: false,
        };
        let (db, vfs, _proc_macro) =
            load_workspace_at(&self.path, &cargo_config, &load_cargo_config, &|_| {})?;
        let host = AnalysisHost::with_database(db);
        let db = host.raw_database();

        let mut visited_files = FxHashSet::default();

        let work = all_modules(db).into_iter().filter(|module| {
            let file_id = module.definition_source_file_id(db).original_file(db);
            let source_root = db.file_source_root(file_id.into());
            let source_root = db.source_root(source_root);
            !source_root.is_library
        });

        for module in work {
            let file_id = module.definition_source_file_id(db).original_file(db);
            if !visited_files.contains(&file_id) {
                let crate_name =
                    module.krate().display_name(db).as_deref().unwrap_or("unknown").to_owned();
                let file_path = vfs.file_path(file_id.into());
                eprintln!("processing crate: {crate_name}, module: {file_path}",);

                let line_index = db.line_index(file_id.into());
                let file_text = db.file_text(file_id.into());

                let mut unresolved_references = find_unresolved_references(&db, file_id.into());
                if !self.include_inactive_code {
                    let mut diagnostics = Vec::new();
                    module.diagnostics(db, &mut diagnostics, false);
                    for diagnostic in diagnostics {
                        let AnyDiagnostic::InactiveCode(inactive_code) = diagnostic else {
                            continue;
                        };

                        let node = inactive_code.node;

                        if node.file_id != file_id {
                            continue;
                        }

                        unresolved_references.retain(|unresolved_reference| {
                            node.value.text_range().contains_range(unresolved_reference.range)
                        });
                    }
                }

                for unresolved_reference in unresolved_references {
                    let line_col = line_index.line_col(unresolved_reference.range.start());
                    let line = line_col.line + 1;
                    let col = line_col.col + 1;
                    let text = &file_text[unresolved_reference.range];
                    println!("{file_path}:{line}:{col}: {text}");
                }

                visited_files.insert(file_id);
            }
        }

        eprintln!();
        eprintln!("scan complete");

        Ok(())
    }
}

fn all_modules(db: &dyn HirDatabase) -> Vec<Module> {
    let mut worklist: Vec<_> =
        Crate::all(db).into_iter().map(|krate| krate.root_module()).collect();
    let mut modules = Vec::new();

    while let Some(module) = worklist.pop() {
        modules.push(module);
        worklist.extend(module.children(db));
    }

    modules
}

#[derive(Debug)]
struct UnresolvedReference {
    range: TextRange,
}

fn find_unresolved_references(db: &RootDatabase, file_id: FileId) -> Vec<UnresolvedReference> {
    let sema = Semantics::new(db);
    let file_id = sema
        .attach_first_edition(file_id)
        .unwrap_or_else(|| EditionedFileId::current_edition(file_id));
    let file = sema.parse(file_id);
    let root = file.syntax();

    let mut unresolved_references = Vec::new();
    for event in root.preorder() {
        let WalkEvent::Enter(element) = event else {
            continue;
        };
        let Some(element) = ast::NameLike::cast(element) else {
            continue;
        };
        let ast::NameLike::NameRef(name_ref) = element else {
            continue;
        };
        if NameRefClass::classify(&sema, &name_ref).is_none() {
            unresolved_references
                .push(UnresolvedReference { range: name_ref.syntax().text_range() })
        }
    }
    unresolved_references
}
