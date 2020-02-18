//! This module handles fuzzy-searching of functions, structs and other symbols
//! by name across the whole workspace and dependencies.
//!
//! It works by building an incrementally-updated text-search index of all
//! symbols. The backbone of the index is the **awesome** `fst` crate by
//! @BurntSushi.
//!
//! In a nutshell, you give a set of strings to `fst`, and it builds a
//! finite state machine describing this set of strings. The strings which
//! could fuzzy-match a pattern can also be described by a finite state machine.
//! What is freaking cool is that you can now traverse both state machines in
//! lock-step to enumerate the strings which are both in the input set and
//! fuzz-match the query. Or, more formally, given two languages described by
//! FSTs, one can build a product FST which describes the intersection of the
//! languages.
//!
//! `fst` does not support cheap updating of the index, but it supports unioning
//! of state machines. So, to account for changing source code, we build an FST
//! for each library (which is assumed to never change) and an FST for each Rust
//! file in the current workspace, and run a query against the union of all
//! those FSTs.

use std::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    mem,
    sync::Arc,
};

use fst::{self, Streamer};
use ra_db::{
    salsa::{self, ParallelDatabase},
    FileId, SourceDatabaseExt, SourceRootId,
};
use ra_syntax::{
    ast::{self, NameOwner},
    match_ast, AstNode, Parse, SmolStr, SourceFile,
    SyntaxKind::{self, *},
    SyntaxNode, SyntaxNodePtr, TextRange, WalkEvent,
};
#[cfg(not(feature = "wasm"))]
use rayon::prelude::*;

use crate::RootDatabase;

#[derive(Debug)]
pub struct Query {
    query: String,
    lowercased: String,
    only_types: bool,
    libs: bool,
    exact: bool,
    limit: usize,
}

impl Query {
    pub fn new(query: String) -> Query {
        let lowercased = query.to_lowercase();
        Query {
            query,
            lowercased,
            only_types: false,
            libs: false,
            exact: false,
            limit: usize::max_value(),
        }
    }

    pub fn only_types(&mut self) {
        self.only_types = true;
    }

    pub fn libs(&mut self) {
        self.libs = true;
    }

    pub fn exact(&mut self) {
        self.exact = true;
    }

    pub fn limit(&mut self, limit: usize) {
        self.limit = limit
    }
}

#[salsa::query_group(SymbolsDatabaseStorage)]
pub trait SymbolsDatabase: hir::db::HirDatabase {
    fn file_symbols(&self, file_id: FileId) -> Arc<SymbolIndex>;
    #[salsa::input]
    fn library_symbols(&self, id: SourceRootId) -> Arc<SymbolIndex>;
    /// The set of "local" (that is, from the current workspace) roots.
    /// Files in local roots are assumed to change frequently.
    #[salsa::input]
    fn local_roots(&self) -> Arc<Vec<SourceRootId>>;
    /// The set of roots for crates.io libraries.
    /// Files in libraries are assumed to never change.
    #[salsa::input]
    fn library_roots(&self) -> Arc<Vec<SourceRootId>>;
}

fn file_symbols(db: &impl SymbolsDatabase, file_id: FileId) -> Arc<SymbolIndex> {
    db.check_canceled();
    let parse = db.parse(file_id);

    let symbols = source_file_to_file_symbols(&parse.tree(), file_id);

    // FIXME: add macros here

    Arc::new(SymbolIndex::new(symbols))
}

pub fn world_symbols(db: &RootDatabase, query: Query) -> Vec<FileSymbol> {
    /// Need to wrap Snapshot to provide `Clone` impl for `map_with`
    struct Snap(salsa::Snapshot<RootDatabase>);
    impl Clone for Snap {
        fn clone(&self) -> Snap {
            Snap(self.0.snapshot())
        }
    }

    let buf: Vec<Arc<SymbolIndex>> = if query.libs {
        let snap = Snap(db.snapshot());
        #[cfg(not(feature = "wasm"))]
        let buf = db
            .library_roots()
            .par_iter()
            .map_with(snap, |db, &lib_id| db.0.library_symbols(lib_id))
            .collect();

        #[cfg(feature = "wasm")]
        let buf = db.library_roots().iter().map(|&lib_id| snap.0.library_symbols(lib_id)).collect();

        buf
    } else {
        let mut files = Vec::new();
        for &root in db.local_roots().iter() {
            let sr = db.source_root(root);
            files.extend(sr.walk())
        }

        let snap = Snap(db.snapshot());
        #[cfg(not(feature = "wasm"))]
        let buf =
            files.par_iter().map_with(snap, |db, &file_id| db.0.file_symbols(file_id)).collect();

        #[cfg(feature = "wasm")]
        let buf = files.iter().map(|&file_id| snap.0.file_symbols(file_id)).collect();

        buf
    };
    query.search(&buf)
}

pub fn index_resolve(db: &RootDatabase, name_ref: &ast::NameRef) -> Vec<FileSymbol> {
    let name = name_ref.text();
    let mut query = Query::new(name.to_string());
    query.exact();
    query.limit(4);
    world_symbols(db, query)
}

#[derive(Default)]
pub struct SymbolIndex {
    symbols: Vec<FileSymbol>,
    map: fst::Map,
}

impl fmt::Debug for SymbolIndex {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SymbolIndex").field("n_symbols", &self.symbols.len()).finish()
    }
}

impl PartialEq for SymbolIndex {
    fn eq(&self, other: &SymbolIndex) -> bool {
        self.symbols == other.symbols
    }
}

impl Eq for SymbolIndex {}

impl Hash for SymbolIndex {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.symbols.hash(hasher)
    }
}

impl SymbolIndex {
    fn new(mut symbols: Vec<FileSymbol>) -> SymbolIndex {
        fn cmp(lhs: &FileSymbol, rhs: &FileSymbol) -> Ordering {
            let lhs_chars = lhs.name.chars().map(|c| c.to_ascii_lowercase());
            let rhs_chars = rhs.name.chars().map(|c| c.to_ascii_lowercase());
            lhs_chars.cmp(rhs_chars)
        }

        #[cfg(not(feature = "wasm"))]
        symbols.par_sort_by(cmp);

        #[cfg(feature = "wasm")]
        symbols.sort_by(cmp);

        let mut builder = fst::MapBuilder::memory();

        let mut last_batch_start = 0;

        for idx in 0..symbols.len() {
            if let Some(next_symbol) = symbols.get(idx + 1) {
                if cmp(&symbols[last_batch_start], next_symbol) == Ordering::Equal {
                    continue;
                }
            }

            let start = last_batch_start;
            let end = idx + 1;
            last_batch_start = end;

            let key = symbols[start].name.as_str().to_ascii_lowercase();
            let value = SymbolIndex::range_to_map_value(start, end);

            builder.insert(key, value).unwrap();
        }

        let map = fst::Map::from_bytes(builder.into_inner().unwrap()).unwrap();
        SymbolIndex { symbols, map }
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn memory_size(&self) -> usize {
        self.map.as_fst().size() + self.symbols.len() * mem::size_of::<FileSymbol>()
    }

    #[cfg(not(feature = "wasm"))]
    pub(crate) fn for_files(
        files: impl ParallelIterator<Item = (FileId, Parse<ast::SourceFile>)>,
    ) -> SymbolIndex {
        let symbols = files
            .flat_map(|(file_id, file)| source_file_to_file_symbols(&file.tree(), file_id))
            .collect::<Vec<_>>();
        SymbolIndex::new(symbols)
    }

    #[cfg(feature = "wasm")]
    pub(crate) fn for_files(
        files: impl Iterator<Item = (FileId, Parse<ast::SourceFile>)>,
    ) -> SymbolIndex {
        let symbols = files
            .flat_map(|(file_id, file)| source_file_to_file_symbols(&file.tree(), file_id))
            .collect::<Vec<_>>();
        SymbolIndex::new(symbols)
    }

    fn range_to_map_value(start: usize, end: usize) -> u64 {
        debug_assert![start <= (std::u32::MAX as usize)];
        debug_assert![end <= (std::u32::MAX as usize)];

        ((start as u64) << 32) | end as u64
    }

    fn map_value_to_range(value: u64) -> (usize, usize) {
        let end = value as u32 as usize;
        let start = (value >> 32) as usize;
        (start, end)
    }
}

impl Query {
    pub(crate) fn search(self, indices: &[Arc<SymbolIndex>]) -> Vec<FileSymbol> {
        let mut op = fst::map::OpBuilder::new();
        for file_symbols in indices.iter() {
            let automaton = fst::automaton::Subsequence::new(&self.lowercased);
            op = op.add(file_symbols.map.search(automaton))
        }
        let mut stream = op.union();
        let mut res = Vec::new();
        while let Some((_, indexed_values)) = stream.next() {
            if res.len() >= self.limit {
                break;
            }
            for indexed_value in indexed_values {
                let symbol_index = &indices[indexed_value.index];
                let (start, end) = SymbolIndex::map_value_to_range(indexed_value.value);

                for symbol in &symbol_index.symbols[start..end] {
                    if self.only_types && !is_type(symbol.ptr.kind()) {
                        continue;
                    }
                    if self.exact && symbol.name != self.query {
                        continue;
                    }
                    res.push(symbol.clone());
                }
            }
        }
        res
    }
}

fn is_type(kind: SyntaxKind) -> bool {
    match kind {
        STRUCT_DEF | ENUM_DEF | TRAIT_DEF | TYPE_ALIAS_DEF => true,
        _ => false,
    }
}

/// The actual data that is stored in the index. It should be as compact as
/// possible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileSymbol {
    pub file_id: FileId,
    pub name: SmolStr,
    pub ptr: SyntaxNodePtr,
    pub name_range: Option<TextRange>,
    pub container_name: Option<SmolStr>,
}

fn source_file_to_file_symbols(source_file: &SourceFile, file_id: FileId) -> Vec<FileSymbol> {
    let mut symbols = Vec::new();
    let mut stack = Vec::new();

    for event in source_file.syntax().preorder() {
        match event {
            WalkEvent::Enter(node) => {
                if let Some(mut symbol) = to_file_symbol(&node, file_id) {
                    symbol.container_name = stack.last().cloned();

                    stack.push(symbol.name.clone());
                    symbols.push(symbol);
                }
            }

            WalkEvent::Leave(node) => {
                if to_symbol(&node).is_some() {
                    stack.pop();
                }
            }
        }
    }

    symbols
}

fn to_symbol(node: &SyntaxNode) -> Option<(SmolStr, SyntaxNodePtr, TextRange)> {
    fn decl<N: NameOwner>(node: N) -> Option<(SmolStr, SyntaxNodePtr, TextRange)> {
        let name = node.name()?;
        let name_range = name.syntax().text_range();
        let name = name.text().clone();
        let ptr = SyntaxNodePtr::new(node.syntax());

        Some((name, ptr, name_range))
    }
    match_ast! {
        match node {
            ast::FnDef(it) => { decl(it) },
            ast::StructDef(it) => { decl(it) },
            ast::EnumDef(it) => { decl(it) },
            ast::TraitDef(it) => { decl(it) },
            ast::Module(it) => { decl(it) },
            ast::TypeAliasDef(it) => { decl(it) },
            ast::ConstDef(it) => { decl(it) },
            ast::StaticDef(it) => { decl(it) },
            _ => None,
        }
    }
}

fn to_file_symbol(node: &SyntaxNode, file_id: FileId) -> Option<FileSymbol> {
    to_symbol(node).map(move |(name, ptr, name_range)| FileSymbol {
        name,
        ptr,
        file_id,
        name_range: Some(name_range),
        container_name: None,
    })
}
