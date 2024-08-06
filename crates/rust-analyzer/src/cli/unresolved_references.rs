use hir::{db::HirDatabase, Crate, HirFileIdExt as _, Module};
use ide::{
    AnalysisHost, AssistResolveStrategy, Diagnostic, DiagnosticsConfig, HighlightConfig,
    HlTag::UnresolvedReference, Severity,
};
use ide_db::{base_db::SourceDatabaseExt as _, FxHashSet, LineIndexDatabase as _};
use load_cargo::{load_workspace_at, LoadCargoConfig, ProcMacroServerChoice};
use project_model::{CargoConfig, RustLibSource};

use crate::cli::flags;

const HL_CONFIG: HighlightConfig = HighlightConfig {
    strings: true,
    punctuation: true,
    specialize_punctuation: true,
    specialize_operator: true,
    operator: true,
    inject_doc_comment: true,
    macro_bang: true,
    syntactic_name_ref_highlighting: false,
};

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
        let analysis = host.analysis();

        let mut found_unresolved_reference = false;
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
                println!(
                    "processing crate: {crate_name}, module: {}",
                    vfs.file_path(file_id.into())
                );

                for range in analysis.highlight(HL_CONFIG, file_id.into()).unwrap() {
                    if range.highlight.tag == UnresolvedReference {
                        found_unresolved_reference = true;

                        println!("{:?}", range.range);
                    }
                }

                visited_files.insert(file_id);
            }
        }

        println!();
        println!("scan complete");

        if found_unresolved_reference {
            println!();
            anyhow::bail!("unresolved reference detected")
        }

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
