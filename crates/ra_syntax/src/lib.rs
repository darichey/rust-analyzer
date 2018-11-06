//! An experimental implementation of [Rust RFC#2256 libsyntax2.0][rfc#2256].
//!
//! The intent is to be an IDE-ready parser, i.e. one that offers
//!
//! - easy and fast incremental re-parsing,
//! - graceful handling of errors, and
//! - maintains all information in the source file.
//!
//! For more information, see [the RFC][rfc#2265], or [the working draft][RFC.md].
//!
//!   [rfc#2256]: <https://github.com/rust-lang/rfcs/pull/2256>
//!   [RFC.md]: <https://github.com/matklad/libsyntax2/blob/master/docs/RFC.md>

#![forbid(
    missing_debug_implementations,
    unconditional_recursion,
    future_incompatible
)]
#![deny(bad_style, missing_docs)]
#![allow(missing_docs)]
//#![warn(unreachable_pub)] // rust-lang/rust#47816

extern crate drop_bomb;
extern crate itertools;
extern crate parking_lot;
extern crate rowan;
extern crate unicode_xid;

#[cfg(test)]
#[macro_use]
extern crate test_utils;

pub mod algo;
pub mod ast;
mod lexer;
#[macro_use]
mod token_set;
mod grammar;
mod parser_api;
mod parser_impl;
mod reparsing;
mod string_lexing;
mod syntax_kinds;
pub mod text_utils;
/// Utilities for simple uses of the parser.
pub mod utils;
mod validation;
mod yellow;

pub use crate::{
    ast::AstNode,
    lexer::{tokenize, Token},
    reparsing::AtomEdit,
    rowan::{SmolStr, TextRange, TextUnit},
    syntax_kinds::SyntaxKind,
    yellow::{
        Direction, OwnedRoot, RefRoot, SyntaxError, SyntaxNode, SyntaxNodeRef, TreeRoot, WalkEvent, Location,
    },
};

use crate::yellow::GreenNode;

// TODO: pick a single name for everything. SourceFile maybe?
/// File represents a parse tree for a single Rust file.
pub type File = ast::RootNode;

impl File {
    fn new(green: GreenNode, errors: Vec<SyntaxError>) -> File {
        let root = SyntaxNode::new(green, errors);
        if cfg!(debug_assertions) {
            utils::validate_block_structure(root.borrowed());
        }
        assert_eq!(root.kind(), SyntaxKind::ROOT);
        ast::RootNode { syntax: root }
    }
    pub fn parse(text: &str) -> File {
        let tokens = tokenize(&text);
        let (green, errors) =
            parser_impl::parse_with(yellow::GreenBuilder::new(), text, &tokens, grammar::root);
        File::new(green, errors)
    }
    pub fn reparse(&self, edit: &AtomEdit) -> File {
        self.incremental_reparse(edit)
            .unwrap_or_else(|| self.full_reparse(edit))
    }
    pub fn incremental_reparse(&self, edit: &AtomEdit) -> Option<File> {
        reparsing::incremental_reparse(self.syntax(), edit, self.errors())
            .map(|(green_node, errors)| File::new(green_node, errors))
    }
    fn full_reparse(&self, edit: &AtomEdit) -> File {
        let text =
            text_utils::replace_range(self.syntax().text().to_string(), edit.delete, &edit.insert);
        File::parse(&text)
    }
    /// Typed AST representation of the parse tree.
    pub fn ast(&self) -> ast::Root {
        self.borrowed()
    }
    /// Untyped homogeneous representation of the parse tree.
    pub fn syntax(&self) -> SyntaxNodeRef {
        self.syntax.borrowed()
    }
    pub fn errors(&self) -> Vec<SyntaxError> {
        let mut errors = self.syntax.root_data().clone();
        errors.extend(validation::validate(self));
        errors
    }
}
