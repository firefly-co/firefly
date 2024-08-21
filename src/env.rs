//! The manager of graphs and entities during a coding session.

use std::{
    collections::{HashMap, HashSet},
    mem,
    path::PathBuf,
};

use tokio::sync::mpsc::Sender;

use crate::{
    analyze::{to_abstract, ScopeTracker},
    errors::Error,
    file::File,
    id::{self, Id},
    parser::parse,
    r#abstract::{Program, TopLevelKind},
    relation::Relations,
    span::{Point, Span, Spanned},
    storage::Storage,
    syntax::SyntaxNode,
};

pub enum Event {
    Recompiled(Id<id::File>),
    Loaded(Id<id::File>),
    Log(String),
}

/// The environment.
pub struct Env {
    pub file_storage: Storage<id::File, File>,
    pub created: Relations<id::Id<id::File>>,
    pub event_sender: Sender<Event>,
    pub visited: HashSet<Id<id::File>>,

    pub uris: HashMap<PathBuf, id::Id<id::File>>,
    pub files: HashMap<id::Id<id::File>, PathBuf>,

    pub open: HashSet<id::Id<id::File>>,
}

impl Env {
    /// Creates a new environment instance.
    pub fn new(event_sender: Sender<Event>) -> Self {
        Self {
            file_storage: Storage::default(),
            created: Relations::default(),
            uris: HashMap::default(),
            files: HashMap::default(),
            event_sender,
            visited: Default::default(),
            open: Default::default(),
        }
    }

    /// Adds a new file to the environment.
    fn add_file(&mut self, path: PathBuf) -> Id<id::File> {
        let file = File::new(SyntaxNode::empty(), "".to_string(), Vec::new());
        let id = self.file_storage.add(file);

        self.created.add_node(id);
        self.uris.insert(path.clone(), id);
        self.files.insert(id, path);

        id
    }

    /// Parses an existing file.
    pub async fn parse_file(&mut self, id: Id<id::File>) -> Option<()> {
        let file = self.file_storage.get_mut(&id)?;
        let (new_ast, errors) = parse(&file.source);

        mem::swap(&mut file.old_tree, &mut file.new_tree);
        file.new_tree = new_ast;
        file.errors = errors;

        Some(())
    }

    /// Applies edits to an existing file.
    pub async fn apply_edits(
        &mut self,
        id: Id<id::File>,
        edits: &[Spanned<String>],
    ) -> Option<&mut File> {
        let file = self.file_storage.get_mut(&id)?;

        for edit in edits {
            let start = edit.span.start.to_offset(&file.source);
            let end = edit.span.end.to_offset(&file.source);

            file.source.replace_range(start..end, &edit.data);
        }
        Some(file)
    }

    /// Ensures a file is present and up to date.
    pub fn ensure_file(&mut self, path: PathBuf, source: String) -> Id<id::File> {
        let path = path.canonicalize().unwrap();

        if let Some(id) = self.uris.get(&path).cloned() {
            id
        } else {
            let id = self.add_file(path);
            self.update_file(id, source);
            id
        }
    }

    /// Updates the source code of an existing file.
    pub fn update_file(&mut self, id: Id<id::File>, source: String) {
        let file = self.file_storage.get_mut(&id).unwrap();
        file.source = source;
    }

    /// Compiles a file and its dependencies asynchronously.
    pub async fn compile(&mut self, id: Id<id::File>) -> Option<()> {
        if self.visited.contains(&id) {
            return None;
        }
        self.visited.insert(id);

        let mut stack = vec![id];
        let mut visited = HashSet::new();
        let mut to_update = HashSet::new();

        while let Some(current_id) = stack.pop() {
            if visited.contains(&current_id) {
                continue;
            }

            visited.insert(current_id);

            let (program, imports) = self.precompile(current_id).await?;
            let new_imports = self.process_imports(&program).await;
            self.update_file_imports(current_id, new_imports.clone(), program);
            let changed = self.update_relations(current_id, &imports, &new_imports);

            to_update.insert(current_id);

            for (id, _) in self.created.get_dependents(current_id) {
                to_update.insert(id);
            }

            stack.extend(changed.into_iter());
        }

        let to_update = self.created.affected(to_update.into_iter());

        for file in to_update {
            self.update_file_errors(file).await;
            self.event_sender.send(Event::Recompiled(file)).await.ok()?;
        }

        Some(())
    }

    /// Handles required data during compilation.
    async fn process_required(&mut self, req: &crate::r#abstract::Require) -> Option<Id<id::File>> {
        let path_str = &req.name.data[1..req.name.data.len() - 1];
        let path = PathBuf::from(path_str);

        if let Ok(resolved_path) = path.canonicalize() {
            self.process_import_path(resolved_path, path).await
        } else {
            None
        }
    }

    /// Precompiles a file.
    async fn precompile(
        &mut self,
        id: Id<id::File>,
    ) -> Option<(crate::r#abstract::Program, HashSet<Id<id::File>>)> {
        self.parse_file(id).await;
        let file = self.file_storage.get_mut(&id)?;
        let (mut data, program) = to_abstract(&file.new_tree);
        file.errors.append(&mut data.errors);
        Some((program, file.imports.clone()))
    }

    /// Processes the import path during compilation.
    async fn process_import_path(
        &mut self,
        resolved_path: PathBuf,
        original_path: PathBuf,
    ) -> Option<Id<id::File>> {
        if resolved_path.exists() && resolved_path.is_file() {
            let source = self.read_file_source(&resolved_path).await?;
            let id = self.ensure_file(original_path, source);
            Some(id)
        } else {
            None
        }
    }

    /// Reads the source code from a file asynchronously.
    async fn read_file_source(&self, path: &PathBuf) -> Option<String> {
        let mut file = tokio::fs::File::open(path).await.ok()?;
        let mut source = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut file, &mut source)
            .await
            .ok()?;
        Some(source)
    }

    /// Processes imports in a program asynchronously.
    async fn process_imports(
        &mut self,
        program: &crate::r#abstract::Program,
    ) -> HashSet<Id<id::File>> {
        let mut new_imports = HashSet::new();

        for top_level in &program.vec {
            if let TopLevelKind::Require(req) = &top_level.data {
                if let Some(id) = self.process_required(req).await {
                    new_imports.insert(id);
                }
            }
        }

        new_imports
    }

    /// Updates file relations after compilation.
    fn update_relations(
        &mut self,
        id: Id<id::File>,
        old_imports: &HashSet<Id<id::File>>,
        new_imports: &HashSet<Id<id::File>>,
    ) -> HashSet<Id<id::File>> {
        let mut affected_files = HashSet::new();

        for removed in old_imports.difference(new_imports) {
            self.created.remove_edge(id, *removed);
            affected_files.insert(*removed);
        }

        for added in new_imports.difference(old_imports) {
            self.created.connect(id, *added);
            affected_files.insert(*added);
        }

        affected_files
    }

    /// Updates file errors and imports after compilation.
    fn update_file_imports(
        &mut self,
        id: Id<id::File>,
        new_imports: HashSet<Id<id::File>>,
        program: Program,
    ) {
        let file = self.file_storage.get_mut(&id).unwrap();
        file.imports = new_imports;
        file.ast = program;
    }

    /// Updates file errors and imports after compilation.
    async fn update_file_errors(&mut self, id: Id<id::File>) {
        let file = self.file_storage.get_mut(&id).unwrap();
        let imps = file.imports.clone();

        let mut errored_defs = HashSet::new();

        let mut tracker = ScopeTracker::new(file.ast.span.clone(), id);
        tracker.register_program(&mut file.ast);

        for id in imps {
            let file = self.file_storage.get_mut(&id).unwrap();
            for name in &file.names {
                tracker
                    .imported
                    .entry(name.data.clone())
                    .or_default()
                    .push((id, name.clone()));
            }
        }

        let file = self.file_storage.get_mut(&id).unwrap();

        for (_, occs) in &tracker.defined {
            if occs.len() > 1 {
                for occ in occs {
                    errored_defs.insert(occ.span.clone());
                    file.errors
                        .push(Error::new("duplicated function.", occ.span.clone()));
                }
            } else {
                let name = &occs[0];
                errored_defs.insert(name.span.clone());
                if tracker.imported.get(&occs[0].data).is_some() {
                    file.errors
                        .push(Error::new("duplicated function.", name.span.clone()));
                }
            }
        }

        let file = self.file_storage.get_mut(&id).unwrap();
        tracker.check_program(&mut file.ast);

        file.names = tracker
            .defined
            .into_values()
            .flatten()
            .collect::<HashSet<_>>();

        file.scopes = tracker.scopes.finish();

        for (_, instances) in tracker.unbound {
            for (on, instance) in instances {
                errored_defs.insert(on.clone());
                file.errors.push(Error::new(
                    format!("cannot find variable `{}`.", instance.data),
                    instance.span,
                ));
            }
        }

        file.errored_tl = errored_defs;
    }

    /// Updates file errors and imports after compilation.
    pub async fn available_names_in_point(
        &mut self,
        id: Id<id::File>,
        point: Point,
    ) -> HashSet<String> {
        let file = self.file_storage.get_mut(&id).unwrap();
        let imps = file.imports.clone();

        let mut tracker = HashSet::<String>::new();
        let mut scopes = Vec::new();

        let span = Span::new(point.clone(), point);

        file.scopes.accumulate(&span, &mut scopes);
        tracker.extend(file.names.clone().into_iter().map(|x| x.data));

        for scope in scopes {
            let res = scope.data.vars.keys().cloned().collect::<Vec<_>>();
            tracker.extend(res)
        }

        for id in imps {
            let file = self.file_storage.get_mut(&id).unwrap();
            tracker.extend(file.names.clone().into_iter().map(|x| x.data))
        }

        tracker
    }
}
