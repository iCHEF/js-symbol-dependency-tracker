use anyhow::Context;
use clap::{Parser, Subcommand};
use dt_core::{
    database::{models, Database, SqliteDb},
    graph::{depend_on_graph::DependOnGraph, used_by_graph::UsedByGraph},
    i18n::{collect_translation, I18nToSymbol},
    parser::{
        anonymous_default_export::SYMBOL_NAME_FOR_ANONYMOUS_DEFAULT_EXPORT,
        collect_symbol_dependency,
        types::{FromOtherModule, FromType, ModuleExport, ModuleScopedVariable, SymbolDependency},
        Input,
    },
    path_resolver::{PathResolver, ToCanonicalString},
    portable::Portable,
    route::{collect_route_dependency, Route, SymbolToRoutes},
    scheduler::ParserCandidateScheduler,
};
use indicatif::{ProgressBar, ProgressStyle};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufReader, Write},
    path::PathBuf,
};

#[derive(Parser)]
#[command(version, about = "Parse a project and serialize its output", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse and export the project in portable format
    Portable {
        /// Input path
        #[arg(short)]
        input: String,

        /// translation.json path
        #[arg(short)]
        translation_path: String,

        /// Output path
        #[arg(short)]
        output: String,
    },

    /// Parse and export the project in database format
    Database {
        /// Input path
        #[arg(short)]
        input: String,

        /// translation.json path
        #[arg(short)]
        translation_path: String,

        /// Output path
        #[arg(short)]
        output: String,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Portable {
            input,
            translation_path,
            output,
        } => {
            parse_and_export_project_to_portable(&input, &output, &translation_path)
                .context("parse and export project to portable")?;
        }
        Command::Database {
            input,
            translation_path,
            output,
        } => {
            parse_and_export_project_to_database(&input, &output, &translation_path)
                .context("parse and export project to database")?;
        }
    }
    Ok(())
}

fn parse_and_export_project_to_portable(
    project_root: &str,
    output_portable_path: &str,
    translation_file_path: &str,
) -> anyhow::Result<()> {
    let project_root = PathBuf::from(project_root).to_canonical_string()?;
    let translation_json = File::open(&translation_file_path)?;
    let translation_json_reader = BufReader::new(translation_json);

    let mut scheduler = ParserCandidateScheduler::new(&project_root);
    let mut depend_on_graph = DependOnGraph::new(&project_root);
    let mut symbol_to_route = SymbolToRoutes::new();
    let mut i18n_to_symbol = I18nToSymbol::new();
    loop {
        match scheduler.get_one_candidate() {
            Some(c) => {
                let module_src = c.to_str().context(format!("to_str() failed: {:?}", c))?;
                let module_ast = Input::Path(module_src).get_module_ast()?;
                let symbol_dependency = collect_symbol_dependency(&module_ast, module_src)?;
                i18n_to_symbol.collect_i18n_usage(module_src, &module_ast)?;
                symbol_to_route.collect_route_dependency(&module_ast, &symbol_dependency)?;

                depend_on_graph.add_symbol_dependency(symbol_dependency)?;
                scheduler.mark_candidate_as_parsed(c);
            }
            None => break,
        }
    }

    let portable = Portable::new(
        project_root.to_owned(),
        serde_json::from_reader(translation_json_reader)?,
        i18n_to_symbol.table,
        symbol_to_route.table,
        UsedByGraph::from(&depend_on_graph),
    );

    let serialized = portable.export()?;
    let mut file = File::create(&output_portable_path)?;
    file.write_all(serialized.as_bytes())?;

    Ok(())
}

fn parse_and_export_project_to_database(
    project_root: &str,
    output_database_path: &str,
    translation_file_path: &str,
) -> anyhow::Result<()> {
    let project_root = PathBuf::from(project_root).to_canonical_string()?;
    // project name "default_project" can be different in feature "cross-project tracing"
    let project =
        Project::new("default_project", &project_root, output_database_path).context(format!(
            "ready to a emit the project to database, project: {}, database: {}",
            project_root, output_database_path
        ))?;

    let translation_file = File::open(translation_file_path).context(format!(
        "open translation file, path: {}",
        translation_file_path
    ))?;
    let translation_json_reader = BufReader::new(translation_file);
    let translation_json: HashMap<String, String> =
        serde_json::from_reader(translation_json_reader).context(format!(
            "deserialize translation file, path: {}",
            translation_file_path
        ))?;
    project
        .add_translation(&translation_json)
        .context("add translation to project")?;

    let mut scheduler = ParserCandidateScheduler::new(&project_root);
    let bar = ProgressBar::new(scheduler.get_total_remaining_candidate_count() as u64);
    bar.set_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}",
        )?
        .progress_chars("##-"),
    );
    loop {
        match scheduler.get_one_candidate() {
            Some(c) => {
                let module_src = c
                    .to_str()
                    .context(format!("get module_src, path_buf: {:?}", c))?;
                let module_ast = Input::Path(module_src)
                    .get_module_ast()
                    .context(format!("get module ast, module_src: {}", module_src))?;

                let symbol_dependency = collect_symbol_dependency(&module_ast, module_src)
                    .context(format!(
                        "collect symbol dependency for module: {}",
                        &module_src
                    ))?;
                let module = project
                    .add_module(&symbol_dependency)
                    .context(format!(
                        "add symbol dependency of module {} to project",
                        symbol_dependency.canonical_path
                    ))
                    .context(format!("add module {} to project", module_src))?;

                let i18n_usage = collect_translation(&module_ast)
                    .context(format!("collect i18n usage for module: {}", &module_src))?;
                project
                    .add_i18n_usage(&module, &i18n_usage)
                    .context(format!(
                        "add i18n usage of module {} to project",
                        module_src
                    ))?;

                let route_usage = collect_route_dependency(&module_ast, &symbol_dependency)
                    .context(format!("collect route usage for module: {}", &module_src))?;
                project
                    .add_route_usage(&module, &route_usage)
                    .context(format!(
                        "add route usage of module {} to project",
                        module_src
                    ))?;

                scheduler.mark_candidate_as_parsed(c);
                bar.inc(1);
            }
            None => break,
        }
    }
    bar.finish_with_message("all modules parsed 🌲");
    Ok(())
}

struct Project {
    db: SqliteDb,
    project_root: String,
    project: models::Project,
    path_resolver: PathResolver,
}

impl Project {
    pub fn new(project_name: &str, project_root: &str, db_path: &str) -> anyhow::Result<Self> {
        let db = SqliteDb::open(db_path)?;
        db.create_tables()?;
        let project = models::Project::create(&db.conn, project_root, project_name)?;
        Ok(Self {
            db,
            project_root: project_root.to_owned(),
            project,
            path_resolver: PathResolver::new(project_root),
        })
    }

    fn remove_prefix(&self, canonical_path: &str) -> String {
        match canonical_path.starts_with(&self.project_root) {
            true => canonical_path[self.project_root.len()..].to_string(),
            false => canonical_path.to_string(),
        }
    }

    fn resolve_path(&self, current_path: &str, import_src: &str) -> anyhow::Result<String> {
        Ok(self.remove_prefix(&self.path_resolver.resolve_path(current_path, import_src)?))
    }

    fn handle_local_variable_table(
        &self,
        module: &models::Module,
        symbol_dependency: &SymbolDependency,
    ) -> anyhow::Result<()> {
        for (
            symbol_name,
            ModuleScopedVariable {
                depend_on,
                import_from,
            },
        ) in symbol_dependency.local_variable_table.iter()
        {
            let current_symbol = module.get_or_create_symbol(
                &self.db.conn,
                models::SymbolVariant::LocalVariable,
                symbol_name,
            )?;
            if let Some(depend_on) = depend_on {
                // Items in depend_on vector is guranteed to be local variables of the same module.
                // So we can create those symbols as local variable.
                for depend_on_symbol_name in depend_on.iter() {
                    let depend_on_symbol = module.get_or_create_symbol(
                        &self.db.conn,
                        models::SymbolVariant::LocalVariable,
                        depend_on_symbol_name,
                    )?;
                    models::SymbolDependency::create(
                        &self.db.conn,
                        &current_symbol,
                        &depend_on_symbol,
                    )?;
                }
            }
            if let Some(FromOtherModule { from, from_type }) = import_from {
                if let Ok(from) = self.resolve_path(&symbol_dependency.canonical_path, &from) {
                    let import_from_module =
                        self.project.get_or_create_module(&self.db.conn, &from)?;
                    // It's ok to create a named export or default export symbol for other module
                    // even that module hasn't been parsed yet.
                    match from_type {
                        dt_core::parser::types::FromType::Named(depend_on_symbol_name) => {
                            let depend_on_symbol = import_from_module.get_or_create_symbol(
                                &self.db.conn,
                                models::SymbolVariant::NamedExport,
                                &depend_on_symbol_name,
                            )?;
                            models::SymbolDependency::create(
                                &self.db.conn,
                                &current_symbol,
                                &depend_on_symbol,
                            )?;
                        }
                        dt_core::parser::types::FromType::Default => {
                            let depend_on_symbol = import_from_module.get_or_create_symbol(
                                &self.db.conn,
                                models::SymbolVariant::DefaultExport,
                                "", // default export doesn't have name
                            )?;
                            models::SymbolDependency::create(
                                &self.db.conn,
                                &current_symbol,
                                &depend_on_symbol,
                            )?;
                        }
                        dt_core::parser::types::FromType::Namespace => {
                            // When A module import namespace from B module, B module is guranteed to be
                            // parsed before A module. So we can query all named exports from B module.
                            let named_export_symbols =
                                import_from_module.get_named_export_symbols(&self.db.conn)?;
                            for depend_on_symbol in named_export_symbols.iter() {
                                if depend_on_symbol.name != SYMBOL_NAME_FOR_ANONYMOUS_DEFAULT_EXPORT
                                {
                                    models::SymbolDependency::create(
                                        &self.db.conn,
                                        &current_symbol,
                                        &depend_on_symbol,
                                    )?;
                                }
                            }
                        }
                    };
                }
            }
        }
        Ok(())
    }

    fn handle_named_export_table(
        &self,
        module: &models::Module,
        symbol_dependency: &SymbolDependency,
    ) -> anyhow::Result<()> {
        for (exported_symbol_name, exported_from) in symbol_dependency.named_export_table.iter() {
            let current_symbol = module.get_or_create_symbol(
                &self.db.conn,
                models::SymbolVariant::NamedExport,
                &exported_symbol_name,
            )?;
            match exported_from {
                ModuleExport::Local(depend_on_symbol_name) => {
                    let depend_on_symbol = module.get_or_create_symbol(
                        &self.db.conn,
                        models::SymbolVariant::LocalVariable,
                        &depend_on_symbol_name,
                    )?;
                    models::SymbolDependency::create(
                        &self.db.conn,
                        &current_symbol,
                        &depend_on_symbol,
                    )?;
                }
                ModuleExport::ReExportFrom(FromOtherModule { from, from_type }) => {
                    if let Ok(from) = self.resolve_path(&symbol_dependency.canonical_path, &from) {
                        let import_from_module =
                            self.project.get_or_create_module(&self.db.conn, &from)?;
                        // It's ok to create a named export or default export symbol for other module
                        // even that module hasn't been parsed yet.
                        match from_type {
                            dt_core::parser::types::FromType::Named(depend_on_symbol_name) => {
                                let depend_on_symbol = import_from_module.get_or_create_symbol(
                                    &self.db.conn,
                                    models::SymbolVariant::NamedExport,
                                    &depend_on_symbol_name,
                                )?;
                                models::SymbolDependency::create(
                                    &self.db.conn,
                                    &current_symbol,
                                    &depend_on_symbol,
                                )?;
                            }
                            dt_core::parser::types::FromType::Default => {
                                let depend_on_symbol = import_from_module.get_or_create_symbol(
                                    &self.db.conn,
                                    models::SymbolVariant::DefaultExport,
                                    "", // default export doesn't have name
                                )?;
                                models::SymbolDependency::create(
                                    &self.db.conn,
                                    &current_symbol,
                                    &depend_on_symbol,
                                )?;
                            }
                            dt_core::parser::types::FromType::Namespace => {
                                // When A module import namespace from B module, B module is guranteed to be
                                // parsed before A module. So we can query all named exports from B module.
                                let named_export_symbols =
                                    import_from_module.get_named_export_symbols(&self.db.conn)?;
                                for depend_on_symbol in named_export_symbols.iter() {
                                    if depend_on_symbol.name
                                        != SYMBOL_NAME_FOR_ANONYMOUS_DEFAULT_EXPORT
                                    {
                                        models::SymbolDependency::create(
                                            &self.db.conn,
                                            &current_symbol,
                                            &depend_on_symbol,
                                        )?;
                                    }
                                }
                            }
                        };
                    }
                }
            }
        }
        Ok(())
    }

    fn handle_default_export(
        &self,
        module: &models::Module,
        symbol_dependency: &SymbolDependency,
    ) -> anyhow::Result<()> {
        if let Some(default_export) = symbol_dependency.default_export.as_ref() {
            let current_symbol = module.get_or_create_symbol(
                &self.db.conn,
                models::SymbolVariant::DefaultExport,
                "", // default export doesn't have name
            )?;
            match default_export {
                ModuleExport::Local(depend_on_symbol_name) => {
                    let depend_on_symbol = module.get_or_create_symbol(
                        &self.db.conn,
                        models::SymbolVariant::LocalVariable,
                        &depend_on_symbol_name,
                    )?;
                    models::SymbolDependency::create(
                        &self.db.conn,
                        &current_symbol,
                        &depend_on_symbol,
                    )?;
                }
                ModuleExport::ReExportFrom(FromOtherModule { from, from_type }) => {
                    if let Ok(from) = self.resolve_path(&symbol_dependency.canonical_path, &from) {
                        let import_from_module =
                            self.project.get_or_create_module(&self.db.conn, &from)?;
                        // It's ok to create a named export or default export symbol for other module
                        // even that module hasn't been parsed yet.
                        match from_type {
                            dt_core::parser::types::FromType::Named(depend_on_symbol_name) => {
                                let depend_on_symbol = import_from_module.get_or_create_symbol(
                                    &self.db.conn,
                                    models::SymbolVariant::NamedExport,
                                    &depend_on_symbol_name,
                                )?;
                                models::SymbolDependency::create(
                                    &self.db.conn,
                                    &current_symbol,
                                    &depend_on_symbol,
                                )?;
                            }
                            dt_core::parser::types::FromType::Default => {
                                let depend_on_symbol = import_from_module.get_or_create_symbol(
                                    &self.db.conn,
                                    models::SymbolVariant::DefaultExport,
                                    "", // default export doesn't have name
                                )?;
                                models::SymbolDependency::create(
                                    &self.db.conn,
                                    &current_symbol,
                                    &depend_on_symbol,
                                )?;
                            }
                            FromType::Namespace => {
                                unreachable!(
                                "can't not export namespace from other module as default export"
                            )
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn handle_re_export_star_from(
        &self,
        module: &models::Module,
        symbol_dependency: &SymbolDependency,
    ) -> anyhow::Result<()> {
        if let Some(re_export_start_from) = symbol_dependency.re_export_star_from.as_ref() {
            for from in re_export_start_from.iter() {
                if let Ok(from) = self.resolve_path(&symbol_dependency.canonical_path, &from) {
                    // When A module do wildcard export from B module, B module is guranteed to be
                    // parsed before A module. So we can query all named exports from B module.
                    let import_from_module = self.project.get_module(&self.db.conn, &from)?;
                    let named_export_symbols =
                        import_from_module.get_named_export_symbols(&self.db.conn)?;
                    for depend_on_symbol in named_export_symbols.iter() {
                        // Create a named export symbol for this module, and set the dependency to
                        // the named export symbol of imported module.
                        if depend_on_symbol.name != SYMBOL_NAME_FOR_ANONYMOUS_DEFAULT_EXPORT {
                            let current_symbol = module.add_symbol(
                                &self.db.conn,
                                models::SymbolVariant::NamedExport,
                                &depend_on_symbol.name,
                            )?;
                            models::SymbolDependency::create(
                                &self.db.conn,
                                &current_symbol,
                                &depend_on_symbol,
                            )?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn add_module(
        &self,
        symbol_dependency: &SymbolDependency,
    ) -> anyhow::Result<models::Module> {
        let module = self.project.get_or_create_module(
            &self.db.conn,
            &self.remove_prefix(&symbol_dependency.canonical_path),
        )?;

        self.handle_local_variable_table(&module, symbol_dependency)?;
        self.handle_named_export_table(&module, symbol_dependency)?;
        self.handle_default_export(&module, symbol_dependency)?;
        self.handle_re_export_star_from(&module, symbol_dependency)?;

        Ok(module)
    }

    pub fn add_translation(
        &self,
        translation_json: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        for (key, value) in translation_json.iter() {
            self.project.add_translation(&self.db.conn, key, value)?;
        }
        Ok(())
    }

    pub fn add_i18n_usage(
        &self,
        module: &models::Module,
        i18n_usage: &HashMap<String, HashSet<String>>,
    ) -> anyhow::Result<()> {
        for (symbol_name, i18n_keys) in i18n_usage.iter() {
            let symbol = module
                .get_symbol(
                    &self.db.conn,
                    models::SymbolVariant::LocalVariable,
                    &symbol_name,
                )
                .context(format!(
                    "try to add i18n keys for symbol {}, but symbol doesn't exist",
                    symbol_name,
                ))?;
            for key in i18n_keys.iter() {
                match self.project.get_translation(&self.db.conn, key) {
                    Ok(translation) => {
                        models::TranslationUsage::create(&self.db.conn, &translation, &symbol)
                            .context(format!(
                                "relate symbol {} to translation {}",
                                symbol_name, key
                            ))?;
                    }
                    Err(_) => {
                        // you can uncomment this to debug
                        // println!("try to add translation for symbol {}, but translation {} doesn't exist", symbol_name, key);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn add_route_usage(
        &self,
        module: &models::Module,
        route_usage: &Vec<Route>,
    ) -> anyhow::Result<()> {
        for Route { path, depend_on } in route_usage.iter() {
            let route = self
                .project
                .add_route(&self.db.conn, path)
                .context(format!("create route {} for project", path))?;
            for symbol_name in depend_on.iter() {
                let symbol = module
                    .get_symbol(
                        &self.db.conn,
                        models::SymbolVariant::LocalVariable,
                        &symbol_name,
                    )
                    .context(format!(
                        "try to add route for symbol {}, but symbol doesn't exist",
                        symbol_name,
                    ))?;
                models::RouteUsage::create(&self.db.conn, &route, &symbol)?;
            }
        }
        Ok(())
    }
}
