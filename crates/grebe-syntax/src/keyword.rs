//! Keyword categories and the identifier rule-override layer.
//!
//! The five keyword lists are vendored alongside the grammar and compiled into
//! static tables by `build.rs`. Counts at snapshot `ce512b8`:
//! reserved 75, unreserved 339, column-name 55, func-name 30, type-name 32.
//!
//! # The override surface (`matcher_factory.cpp` `AddRuleOverride`)
//!
//! ~25 lexical rules are overridden in C++ at runtime, and the `.gram` text
//! carries only simplified stubs for them — e.g. `StringLiteral <- '\'' [^\']* '\''`
//! with no `''` doubling, and `OperatorLiteral <- Identifier`. Reproducing the
//! shipping parser means implementing the overrides, not the stubs.
//!
//! `IdentifierMatcher` semantics, from `identifier_matcher.hpp`: a word matches
//! a rule if it is **no keyword at all**, an **unreserved** keyword, or a
//! keyword in **that rule's allowed category**. Reserved\* rule variants accept
//! any word.
//!
//! One special case worth calling out because it looks like a bug and is not:
//! `TableName` additionally accepts **single-quoted strings**. That is precisely
//! how `FROM 'file.csv'` parses as a table reference.

/// Which keyword category a rule admits beyond the unreserved set.
///
/// The default for an identifier rule is [`Category::ColumnName`];
/// `TypeName` rules take [`Category::TypeName`]; `Function` and
/// `TableFunctionName` rules take [`Category::TypeFunc`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    ColumnName,
    TypeName,
    /// Function-name keywords *and* type-name keywords — the `TYPE_FUNC` bucket.
    TypeFunc,
    /// Reserved\* rule variants: any word matches, keyword or not.
    Any,
}

/// How a word is classified against the vendored keyword lists.
///
/// A **bitmask**, not a single category: the five `.list` files are NOT
/// disjoint. `map`/`struct`/`tuple`/`generated` are both column-name and
/// func-name keywords; `columns`/`try_cast` are both column-name and
/// type-name; 26 words are both func-name and type-name. Collapsing a word
/// to one category would make `SELECT map(...)` unparseable.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Keyword(pub u8);

impl Keyword {
    /// Not a keyword at all. Always a valid identifier.
    pub const NONE: Self = Self(0);
    /// Usable as an identifier anywhere.
    pub const UNRESERVED: Self = Self(1 << 0);
    pub const COLUMN_NAME: Self = Self(1 << 1);
    pub const FUNC_NAME: Self = Self(1 << 2);
    pub const TYPE_NAME: Self = Self(1 << 3);
    /// Never a bare identifier.
    pub const RESERVED: Self = Self(1 << 4);

    #[must_use]
    pub const fn has(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }

    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Does a word of this classification satisfy an identifier rule admitting
    /// `category`? This is the whole of `IdentifierMatcher`'s decision:
    /// a word matches if it is no keyword, an unreserved keyword, or a keyword
    /// in that rule's allowed category.
    #[must_use]
    pub const fn matches(self, category: Category) -> bool {
        if matches!(category, Category::Any) {
            return true;
        }
        if self.is_none() || self.has(Self::UNRESERVED) {
            return true;
        }
        match category {
            Category::ColumnName => self.has(Self::COLUMN_NAME),
            Category::TypeName => self.has(Self::TYPE_NAME),
            Category::TypeFunc => self.has(Self::TYPE_NAME) || self.has(Self::FUNC_NAME),
            Category::Any => true,
        }
    }
}
