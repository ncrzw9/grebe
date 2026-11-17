//! Compiles the vendored PEG grammar into Rust matcher tables.
//!
//! Runs at build time so that nothing parses grammar text at runtime.
//! Reads `vendor/grammar/**` — verbatim DuckDB, never
//! hand-edited — and writes `$OUT_DIR/grammar_tables.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Category {
    ColumnName,
    TypeName,
    TypeFunc,
    Any,
    BareLabel,
}

#[derive(Clone, Debug, PartialEq)]
enum PegToken {
    Name(String),
    Literal(String),
    Arrow,    // <-
    Slash,    // /
    LParen,   // (
    RParen,   // )
    Question, // ?
    Star,     // *
    Plus,     // +
    Excl,     // !
    Amp,      // &
    Comma,    // ,
    Regex(String),
}

fn tokenize_gram(src: &str) -> Vec<PegToken> {
    let mut tokens = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    let n = bytes.len();
    while i < n {
        let c = bytes[i] as char;
        if c == '#' {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '<' && i + 1 < n && bytes[i + 1] == b'-' {
            tokens.push(PegToken::Arrow);
            i += 2;
            continue;
        }
        // `[...]` character classes and `<...>` token captures are kept as one
        // opaque token each; see `Parser::atom` for why their content is unused.
        if c == '[' || c == '<' {
            let final_char = if c == '[' { ']' } else { '>' };
            let mut j = i + 1;
            let mut rx = String::new();
            rx.push(c);
            while j < n {
                if bytes[j] as char == final_char {
                    rx.push(final_char);
                    j += 1;
                    break;
                }
                if bytes[j] == b'\\' && j + 1 < n {
                    rx.push('\\');
                    rx.push(bytes[j + 1] as char);
                    j += 2;
                } else {
                    rx.push(bytes[j] as char);
                    j += 1;
                }
            }
            tokens.push(PegToken::Regex(rx));
            i = j;
            continue;
        }
        if c == '\'' {
            let mut j = i + 1;
            let mut lit = String::new();
            while j < n {
                if bytes[j] == b'\'' {
                    j += 1;
                    break;
                }
                if bytes[j] == b'\\' && j + 1 < n {
                    lit.push(bytes[j + 1] as char);
                    j += 2;
                } else {
                    lit.push(bytes[j] as char);
                    j += 1;
                }
            }
            tokens.push(PegToken::Literal(lit));
            i = j;
            continue;
        }
        if c.is_alphanumeric() || c == '_' || c == '%' {
            let mut j = i + 1;
            while j < n && ((bytes[j] as char).is_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let name = std::str::from_utf8(&bytes[i..j]).unwrap().to_string();
            tokens.push(PegToken::Name(name));
            i = j;
            continue;
        }
        match c {
            '/' => tokens.push(PegToken::Slash),
            '(' => tokens.push(PegToken::LParen),
            ')' => tokens.push(PegToken::RParen),
            '?' => tokens.push(PegToken::Question),
            '*' => tokens.push(PegToken::Star),
            '+' => tokens.push(PegToken::Plus),
            '!' => tokens.push(PegToken::Excl),
            '&' => tokens.push(PegToken::Amp),
            ',' => tokens.push(PegToken::Comma),
            _ => panic!("unexpected char in grammar: '{}' at index {}", c, i),
        }
        i += 1;
    }
    tokens
}

#[derive(Clone, Debug)]
enum PegExpr {
    Literal(String),
    Ref(String),
    Call(String, Box<PegExpr>),
    Seq(Vec<PegExpr>),
    Choice(Vec<PegExpr>),
    Optional(Box<PegExpr>),
    ZeroOrMore(Box<PegExpr>),
    OneOrMore(Box<PegExpr>),
    Not(Box<PegExpr>),
    And(Box<PegExpr>),
    Identifier(Category),
    EndOfInput,
    Dummy,
}

struct Parser {
    tokens: Vec<PegToken>,
    i: usize,
}

impl Parser {
    fn new(tokens: Vec<PegToken>) -> Self {
        Self { tokens, i: 0 }
    }

    fn peek(&self) -> Option<&PegToken> {
        self.tokens.get(self.i)
    }

    fn next(&mut self) -> Option<PegToken> {
        if self.i < self.tokens.len() {
            let t = self.tokens[self.i].clone();
            self.i += 1;
            Some(t)
        } else {
            None
        }
    }

    fn build(mut self) -> PegExpr {
        let e = self.parse_expression();
        if self.i != self.tokens.len() {
            panic!("trailing tokens: {:?}", &self.tokens[self.i..]);
        }
        e
    }

    fn parse_expression(&mut self) -> PegExpr {
        self.choice()
    }

    fn choice(&mut self) -> PegExpr {
        let mut alts = vec![self.seq()];
        while let Some(PegToken::Slash) = self.peek() {
            self.next();
            alts.push(self.seq());
        }
        if alts.len() == 1 {
            alts.pop().unwrap()
        } else {
            PegExpr::Choice(alts)
        }
    }

    fn seq(&mut self) -> PegExpr {
        let mut items = Vec::new();
        while let Some(t) = self.peek() {
            if matches!(t, PegToken::Slash | PegToken::RParen) {
                break;
            }
            items.push(self.prefixed());
        }
        if items.is_empty() {
            panic!("empty sequence");
        }
        if items.len() == 1 {
            items.pop().unwrap()
        } else {
            PegExpr::Seq(items)
        }
    }

    fn prefixed(&mut self) -> PegExpr {
        if let Some(t) = self.peek() {
            match t {
                PegToken::Excl => {
                    self.next();
                    PegExpr::Not(Box::new(self.prefixed()))
                }
                PegToken::Amp => {
                    self.next();
                    PegExpr::And(Box::new(self.prefixed()))
                }
                _ => self.postfixed(),
            }
        } else {
            panic!("expected expression");
        }
    }

    fn postfixed(&mut self) -> PegExpr {
        let mut e = self.atom();
        while let Some(t) = self.peek() {
            match t {
                PegToken::Question => {
                    self.next();
                    e = PegExpr::Optional(Box::new(e));
                }
                PegToken::Star => {
                    self.next();
                    e = PegExpr::ZeroOrMore(Box::new(e));
                }
                PegToken::Plus => {
                    self.next();
                    e = PegExpr::OneOrMore(Box::new(e));
                }
                _ => break,
            }
        }
        e
    }

    fn atom(&mut self) -> PegExpr {
        let t = self.next().expect("expected token");
        match t {
            PegToken::Literal(lit) => PegExpr::Literal(lit),
            PegToken::Name(name) => {
                // The grammar defines exactly these two macros, and a call is
                // the macro name followed directly by `(`.
                if (name == "List" || name == "Parens") && self.peek() == Some(&PegToken::LParen) {
                    self.next();
                    let arg = self.parse_expression();
                    assert_eq!(
                        self.next(),
                        Some(PegToken::RParen),
                        "expected ) in macro call"
                    );
                    PegExpr::Call(name, Box::new(arg))
                } else {
                    PegExpr::Ref(name)
                }
            }
            PegToken::LParen => {
                let e = self.parse_expression();
                assert_eq!(self.next(), Some(PegToken::RParen), "expected )");
                e
            }
            // Character classes and `<...>` captures occur only in the lexical
            // stubs that the `overrides` table discards, and in the unreferenced
            // `%whitespace` directive, so their content is never needed.
            PegToken::Regex(_) => PegExpr::Dummy,
            _ => panic!("unexpected token in atom: {:?}", t),
        }
    }
}

struct RuleDef {
    name: String,
    param: Option<String>,
    expr: PegExpr,
}

fn parse_gram_file(src: &str) -> Vec<RuleDef> {
    let tokens = tokenize_gram(src);
    let mut rules = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let name = match &tokens[i] {
            PegToken::Name(n) => n.clone(),
            _ => panic!("expected rule name, got {:?}", tokens[i]),
        };
        i += 1;
        let mut param = None;
        if i < tokens.len() && tokens[i] == PegToken::LParen {
            i += 1;
            param = match &tokens[i] {
                PegToken::Name(p) => Some(p.clone()),
                _ => panic!("expected parameter name"),
            };
            i += 1;
            assert_eq!(tokens[i], PegToken::RParen);
            i += 1;
        }
        assert_eq!(tokens[i], PegToken::Arrow, "expected <- for rule {}", name);
        i += 1;
        let mut expr_tokens = Vec::new();
        while i < tokens.len() {
            // A rule body runs until the next `Name <-` or `Name(Param) <-`.
            if let PegToken::Name(_) = &tokens[i] {
                let is_rule_def = (i + 1 < tokens.len() && tokens[i + 1] == PegToken::Arrow)
                    || (i + 4 < tokens.len()
                        && tokens[i + 1] == PegToken::LParen
                        && tokens[i + 4] == PegToken::Arrow);
                if is_rule_def {
                    break;
                }
            }
            expr_tokens.push(tokens[i].clone());
            i += 1;
        }
        let expr = Parser::new(expr_tokens).build();
        rules.push(RuleDef { name, param, expr });
    }
    rules
}

fn collect_refs<'a>(expr: &'a PegExpr, out: &mut std::collections::HashSet<&'a str>) {
    match expr {
        PegExpr::Ref(name) => {
            out.insert(name.as_str());
        }
        PegExpr::Call(_, arg) => collect_refs(arg, out),
        PegExpr::Seq(items) | PegExpr::Choice(items) => {
            for item in items {
                collect_refs(item, out);
            }
        }
        PegExpr::Optional(item)
        | PegExpr::ZeroOrMore(item)
        | PegExpr::OneOrMore(item)
        | PegExpr::Not(item)
        | PegExpr::And(item) => collect_refs(item, out),
        PegExpr::Literal(_) | PegExpr::Identifier(_) | PegExpr::EndOfInput | PegExpr::Dummy => {}
    }
}

fn expand_macro(expr: &PegExpr, param_name: &str, arg: &PegExpr) -> PegExpr {
    match expr {
        PegExpr::Literal(s) => PegExpr::Literal(s.clone()),
        PegExpr::Ref(name) => {
            if name == param_name {
                arg.clone()
            } else {
                PegExpr::Ref(name.clone())
            }
        }
        PegExpr::Call(name, sub_arg) => {
            let expanded_arg = expand_macro(sub_arg, param_name, arg);
            PegExpr::Call(name.clone(), Box::new(expanded_arg))
        }
        PegExpr::Seq(items) => PegExpr::Seq(
            items
                .iter()
                .map(|item| expand_macro(item, param_name, arg))
                .collect(),
        ),
        PegExpr::Choice(items) => PegExpr::Choice(
            items
                .iter()
                .map(|item| expand_macro(item, param_name, arg))
                .collect(),
        ),
        PegExpr::Optional(item) => PegExpr::Optional(Box::new(expand_macro(item, param_name, arg))),
        PegExpr::ZeroOrMore(item) => {
            PegExpr::ZeroOrMore(Box::new(expand_macro(item, param_name, arg)))
        }
        PegExpr::OneOrMore(item) => {
            PegExpr::OneOrMore(Box::new(expand_macro(item, param_name, arg)))
        }
        PegExpr::Not(item) => PegExpr::Not(Box::new(expand_macro(item, param_name, arg))),
        PegExpr::And(item) => PegExpr::And(Box::new(expand_macro(item, param_name, arg))),
        PegExpr::Identifier(cat) => PegExpr::Identifier(*cat),
        PegExpr::EndOfInput => PegExpr::EndOfInput,
        PegExpr::Dummy => PegExpr::Dummy,
    }
}

fn expand_calls(expr: &PegExpr, macros: &HashMap<String, (String, PegExpr)>) -> PegExpr {
    match expr {
        PegExpr::Literal(s) => PegExpr::Literal(s.clone()),
        PegExpr::Ref(name) => PegExpr::Ref(name.clone()),
        PegExpr::Call(name, arg) => {
            let expanded_arg = expand_calls(arg, macros);
            if let Some((param, body)) = macros.get(name) {
                let substituted = expand_macro(body, param, &expanded_arg);
                expand_calls(&substituted, macros)
            } else {
                panic!("undefined macro: {} in expression: {:?}", name, expr);
            }
        }
        PegExpr::Seq(items) => PegExpr::Seq(
            items
                .iter()
                .map(|item| expand_calls(item, macros))
                .collect(),
        ),
        PegExpr::Choice(items) => PegExpr::Choice(
            items
                .iter()
                .map(|item| expand_calls(item, macros))
                .collect(),
        ),
        PegExpr::Optional(item) => PegExpr::Optional(Box::new(expand_calls(item, macros))),
        PegExpr::ZeroOrMore(item) => PegExpr::ZeroOrMore(Box::new(expand_calls(item, macros))),
        PegExpr::OneOrMore(item) => PegExpr::OneOrMore(Box::new(expand_calls(item, macros))),
        PegExpr::Not(item) => PegExpr::Not(Box::new(expand_calls(item, macros))),
        PegExpr::And(item) => PegExpr::And(Box::new(expand_calls(item, macros))),
        PegExpr::Identifier(cat) => PegExpr::Identifier(*cat),
        PegExpr::EndOfInput => PegExpr::EndOfInput,
        PegExpr::Dummy => PegExpr::Dummy,
    }
}

struct Compiler {
    expressions: Vec<String>,
    rule_ids: HashMap<String, u32>,
    /// The same expressions as data, for the FIRST-set computation.
    shapes: Vec<Shape>,
}

/// What a compiled expression is made of, by index into the expression
/// table.
#[derive(Clone, Debug)]
enum Shape {
    Literal(String),
    Rule(String),
    Identifier,
    Seq(Vec<usize>),
    Choice(Vec<usize>),
    /// `?` and `*`: may match nothing.
    Maybe(usize),
    Plus(usize),
    /// Matches without consuming: lookahead, end of input.
    ZeroWidth,
    /// The body of a rule the matcher implements natively.
    Native,
}

// What kind of token an expression can begin with. `FIRST_NULLABLE` means it
// can also match without consuming one. The same bits are in matcher.rs.
const FIRST_NULLABLE: u8 = 1;
const FIRST_WORD: u8 = 1 << 1;
const FIRST_QUOTED_IDENT: u8 = 1 << 2;
const FIRST_STRING: u8 = 1 << 3;
const FIRST_NUMBER: u8 = 1 << 4;
const FIRST_OPERATOR: u8 = 1 << 5;

/// The FIRST set of each rule the matcher implements natively: the token
/// kinds its code in matcher.rs accepts. A native rule missing here fails
/// the build, so a new one cannot go unclassified.
fn native_first(rule: &str) -> u8 {
    match rule {
        "StringLiteral" => FIRST_STRING,
        "NumberLiteral" => FIRST_NUMBER,
        "OperatorLiteral" | "AnyOperatorLiteral" => FIRST_OPERATOR,
        "ReservedKeyword" | "UnreservedKeyword" | "ColumnNameKeyword" | "FuncNameKeyword"
        | "TypeNameKeyword" | "BareLabelKeyword" => FIRST_WORD,
        "NewlineBefore" | "EndOfInput" => FIRST_NULLABLE,
        _ => panic!("native rule `{rule}` has no FIRST set; add it to `native_first`"),
    }
}

/// For every expression: the token kinds and the literals a match can begin
/// with. A fixed point over the rule graph, starting from nothing; a set
/// only grows, so it ends.
///
/// The matcher consults this before trying an expression. Most
/// alternatives of most choices cannot start with the token at hand -- the
/// precedence chain alone is twenty rules deep under every value -- and
/// skipping them is a table lookup instead of a descent that ends in a
/// failed comparison. A set that is too large only costs time; one that is
/// too small would reject valid SQL, so every case below errs large.
fn first_sets(
    shapes: &[Shape],
    rule_roots: &HashMap<String, usize>,
    literal_ids: &HashMap<String, u16>,
) -> Vec<(u8, std::collections::BTreeSet<u16>)> {
    let mut first: Vec<(u8, std::collections::BTreeSet<u16>)> =
        vec![(0, Default::default()); shapes.len()];
    loop {
        let mut changed = false;
        for i in (0..shapes.len()).rev() {
            let mut flags = 0u8;
            let mut lits = std::collections::BTreeSet::new();
            match &shapes[i] {
                Shape::Literal(l) => {
                    lits.insert(literal_ids[&l.to_ascii_lowercase()]);
                }
                Shape::Identifier => flags = FIRST_WORD | FIRST_QUOTED_IDENT,
                Shape::ZeroWidth => flags = FIRST_NULLABLE,
                Shape::Native => {} // set through the rule that owns it
                Shape::Rule(name) => {
                    let root = rule_roots[name];
                    if matches!(shapes[root], Shape::Native) {
                        flags = native_first(name);
                    } else {
                        flags = first[root].0;
                        lits = first[root].1.clone();
                    }
                }
                Shape::Seq(items) => {
                    flags = FIRST_NULLABLE;
                    for &item in items {
                        flags |= first[item].0 & !FIRST_NULLABLE;
                        lits.extend(first[item].1.iter().copied());
                        if first[item].0 & FIRST_NULLABLE == 0 {
                            flags &= !FIRST_NULLABLE;
                            break;
                        }
                    }
                }
                Shape::Choice(items) => {
                    for &item in items {
                        flags |= first[item].0;
                        lits.extend(first[item].1.iter().copied());
                    }
                }
                Shape::Maybe(item) => {
                    flags = first[*item].0 | FIRST_NULLABLE;
                    lits = first[*item].1.clone();
                }
                Shape::Plus(item) => {
                    flags = first[*item].0;
                    lits = first[*item].1.clone();
                }
            }
            if (flags, &lits) != (first[i].0, &first[i].1) {
                first[i] = (flags, lits);
                changed = true;
            }
        }
        if !changed {
            return first;
        }
    }
}

impl Compiler {
    fn compile_expr(&mut self, expr: &PegExpr) -> usize {
        let idx = self.expressions.len();
        self.expressions.push(String::new()); // placeholder
        self.shapes.push(Shape::ZeroWidth); // placeholder
        let ids = |items: &[usize]| {
            items
                .iter()
                .map(|i| format!("ExprId({i})"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let (compiled, shape) = match expr {
            PegExpr::Literal(s) => (format!("Expr::Literal({:?})", s), Shape::Literal(s.clone())),
            PegExpr::Ref(name) => {
                if name == "EndOfInput" {
                    ("Expr::EndOfInput".to_string(), Shape::ZeroWidth)
                } else if let Some(&id) = self.rule_ids.get(name) {
                    (
                        format!("Expr::Rule(RuleId({}))", id),
                        Shape::Rule(name.clone()),
                    )
                } else {
                    panic!("undefined reference to rule: {}", name);
                }
            }
            PegExpr::Seq(items) => {
                let items: Vec<usize> = items.iter().map(|item| self.compile_expr(item)).collect();
                (format!("Expr::Seq(&[{}])", ids(&items)), Shape::Seq(items))
            }
            PegExpr::Choice(items) => {
                let items: Vec<usize> = items.iter().map(|item| self.compile_expr(item)).collect();
                (
                    format!("Expr::Choice(&[{}])", ids(&items)),
                    Shape::Choice(items),
                )
            }
            PegExpr::Optional(item) => {
                let item = self.compile_expr(item);
                (
                    format!("Expr::Optional(ExprId({item}))"),
                    Shape::Maybe(item),
                )
            }
            PegExpr::ZeroOrMore(item) => {
                let item = self.compile_expr(item);
                (
                    format!("Expr::ZeroOrMore(ExprId({item}))"),
                    Shape::Maybe(item),
                )
            }
            PegExpr::OneOrMore(item) => {
                let item = self.compile_expr(item);
                (
                    format!("Expr::OneOrMore(ExprId({item}))"),
                    Shape::Plus(item),
                )
            }
            PegExpr::Not(item) => {
                let item = self.compile_expr(item);
                (format!("Expr::Not(ExprId({item}))"), Shape::ZeroWidth)
            }
            PegExpr::And(item) => {
                let item = self.compile_expr(item);
                (format!("Expr::And(ExprId({item}))"), Shape::ZeroWidth)
            }
            PegExpr::Identifier(cat) => {
                let cat_str = match cat {
                    Category::ColumnName => "Category::ColumnName",
                    Category::TypeName => "Category::TypeName",
                    Category::TypeFunc => "Category::TypeFunc",
                    Category::Any => "Category::Any",
                    Category::BareLabel => "Category::BareLabel",
                };
                (
                    format!("Expr::Identifier(crate::keyword::{})", cat_str),
                    Shape::Identifier,
                )
            }
            PegExpr::EndOfInput => ("Expr::EndOfInput".to_string(), Shape::ZeroWidth),
            // A placeholder the matcher never evaluates: every `Dummy` rule is
            // dispatched by name in `Matcher::evaluate_rule` before its
            // expression is consulted.
            PegExpr::Dummy => ("Expr::EndOfInput".to_string(), Shape::Native),
            PegExpr::Call(_, _) => panic!("macro calls should have been expanded"),
        };
        self.expressions[idx] = compiled;
        self.shapes[idx] = shape;
        idx
    }
}

/// Corrections for places where the vendored grammar text accepts SQL that the
/// shipping DuckDB parser rejects.
///
/// These sit apart from the `overrides` table in `main`, which supplies the
/// lexical semantics the grammar text deliberately leaves to its consumer.
/// These are different in kind: the grammar and the engine genuinely
/// disagree, and we follow the engine, because a linter that accepts what
/// DuckDB will refuse to run reports nothing where the user most needs a
/// diagnostic.
///
/// `vendor/` is verbatim and never hand-edited, so the correction
/// lives here — and as a *transformation* of the parsed rule, not a literal
/// replacement of it. A literal replacement goes stale in silence: upstream
/// adds a statement kind, our hardcoded copy keeps the old list, and nothing
/// says so. Each transform below asserts the shape it expects and panics the
/// build if the vendored rule has moved. A grammar refresh is already a
/// reviewed change, so a loud failure there is exactly where this should
/// surface.
fn apply_structural_overrides(rules: &mut HashMap<String, PegExpr>) {
    // `LiteralExpression <- StringLiteral / NumberLiteral / ConstantLiteral`
    // (statements/expression.gram:28). Adjacent string literals concatenate in
    // standard SQL and in DuckDB -- `SELECT 'Hello' ' ' 'World'` is one value
    // -- but the vendored grammar has no rule for it anywhere, so every such
    // statement would be a false `PRS001` on valid SQL.
    //
    // Widened only in the literal-EXPRESSION position, not on `StringLiteral`
    // itself: that rule is also reached by `TypeLiteral <- Type StringLiteral`,
    // `IntervalStringParameter` and `ExtractStringArgument`, where a run of
    // strings is not meaningful and accepting one would be pure over-acceptance.
    let lit = rules
        .get_mut("LiteralExpression")
        .expect("vendored grammar has no `LiteralExpression` rule");
    let PegExpr::Choice(alts) = lit else {
        panic!("`LiteralExpression` is no longer a choice; this override is stale");
    };
    let sidx = alts
        .iter()
        .position(|a| matches!(a, PegExpr::Ref(n) if n == "StringLiteral"))
        .expect("`LiteralExpression` no longer offers a bare `StringLiteral`");
    // Not `StringLiteral+`: DuckDB inherits Postgres's rule that adjacent
    // string literals only concatenate across a NEWLINE. `SELECT 'a' 'b'` on
    // one line is a syntax error, `SELECT 'a'\n'b'` is one value. `NewlineBefore`
    // is a zero-width matcher-level guard asking about the trivia between two
    // tokens, which a PEG shape cannot express; every literal is still matched
    // by the real `StringLiteral` rule, so the CST keeps `StringLiteral` nodes
    // for rules that read them.
    alts[sidx] = continued_string();

    // The engine does the same wherever a string literal is matched, not
    // only in an expression: its string matcher consumes the continuation
    // lines itself, so `DATE '2020-'\n'01-01'` and a continued COPY path are
    // one literal too. Every other reference gets the same shape. `TableName`
    // is left alone: there the engine matches a string token as an
    // identifier, with no continuation.
    let mut widened = 0;
    for (name, expr) in rules.iter_mut() {
        if name == "LiteralExpression" || name == "TableName" {
            continue;
        }
        widened += widen_string_literals(expr);
    }
    assert!(
        widened > 0,
        "no rule references `StringLiteral` any more; this override is stale"
    );

    // (`CopyFileName` used to be reordered here, longest alternative first,
    // so that `COPY t TO out.csv` could reach the dotted form. The v2.0.0
    // grammar lists `CopyFileNameIdentifierColId` before
    // `CopyFileNameIdentifier` itself, so the override is gone; the
    // differential corpus has the statements that needed it.)

    // `SelectClause <- 'SELECT' DistinctClause? TargetList?`
    // (statements/select.gram). The grammar makes the selection list
    // optional; both shipping engines raise "SELECT clause without selection
    // list". Drop the `?`.
    //
    // Checked against 1.5.5 and 2.0.0: both reject it, so it is a grammar
    // bug, and following the engines costs us nothing on either gate.
    //
    // (The other member of this class was `Statement <- ... /
    // ExpressionStatement`, a bare expression as a statement. The v2.0.0
    // grammar dropped the alternative itself.)
    let sel = rules
        .get_mut("SelectClause")
        .expect("vendored grammar has no `SelectClause` rule");
    let PegExpr::Seq(items) = sel else {
        panic!("`SelectClause` is no longer a sequence; this override is stale");
    };
    let mut unwrapped = 0;
    for item in items.iter_mut() {
        let PegExpr::Optional(inner) = item else {
            continue;
        };
        if matches!(&**inner, PegExpr::Ref(n) if n == "TargetList") {
            *item = PegExpr::Ref("TargetList".to_string());
            unwrapped += 1;
        }
    }
    assert_eq!(
        unwrapped, 1,
        "`SelectClause` no longer has exactly one optional `TargetList`; \
         this override is stale"
    );

    // `ComparisonExpression <- BetweenInLikeExpression ComparisonExpressionTail*`
    // and `IsDistinctFromExpression <- ComparisonExpression IsDistinctFromTail*`
    // (statements/expression.gram). The grammar text takes any number of
    // tails; the engine's transformer then refuses more than one of either
    // with "Chained comparisons are not supported" (transform_expression.cpp),
    // and 1.5.5 refuses the same statements as plain syntax errors. So
    // `WHERE 1 < x < 10` does not run on any supported engine, and a linter
    // that parses it has nothing to say about a classic mistake. At most one
    // tail: `*` becomes `?`.
    for (rule, tail) in [
        ("ComparisonExpression", "ComparisonExpressionTail"),
        ("IsDistinctFromExpression", "IsDistinctFromTail"),
    ] {
        let expr = rules
            .get_mut(rule)
            .unwrap_or_else(|| panic!("vendored grammar has no `{rule}` rule"));
        let PegExpr::Seq(items) = expr else {
            panic!("`{rule}` is no longer a sequence; this override is stale");
        };
        let mut narrowed = 0;
        for item in items.iter_mut() {
            let PegExpr::ZeroOrMore(inner) = item else {
                continue;
            };
            if matches!(&**inner, PegExpr::Ref(n) if n == tail) {
                *item = PegExpr::Optional(Box::new(PegExpr::Ref(tail.to_string())));
                narrowed += 1;
            }
        }
        assert_eq!(
            narrowed, 1,
            "`{rule}` no longer has exactly one `{tail}*`; this override is stale"
        );
    }

    // A bare expression as a statement: `1 + 1;`, `42 AS answer, 'x';`.
    //
    // The grammar up to 1.5 had `Statement <- ... / ExpressionStatement`; the
    // v2.0.0 grammar directory does not, because 2.0 moved the rule out of
    // the core grammar and into the CLI, which adds it when it starts.
    // Checked against 2.0.0: the `duckdb` shell runs `1+1;` and
    // `CREATE TABLE z AS 1;`, and the library (`duckdb.extract_statements`)
    // refuses both; 1.5.5 refuses them everywhere. The shell is how SQL files
    // are run, by hand and by the editor extension, so a file it accepts must
    // not be a `PRS001`: the rule is kept, as the last alternative, where it
    // can only match what nothing else did. That it is not valid through a
    // client library is a fact about where the file runs, not its syntax.
    let stmt = rules
        .get_mut("Statement")
        .expect("vendored grammar has no `Statement` rule");
    let PegExpr::Choice(alts) = stmt else {
        panic!("`Statement` is no longer a choice; this override is stale");
    };
    assert!(
        !alts
            .iter()
            .any(|a| matches!(a, PegExpr::Ref(n) if n == "ExpressionStatement")),
        "upstream has `ExpressionStatement` again; drop this override"
    );
    alts.push(PegExpr::Ref("ExpressionStatement".to_string()));
    let alias = PegExpr::Choice(
        ["ColIdExpression", "ExpressionAsCollabel", "Expression"]
            .into_iter()
            .map(|n| PegExpr::Ref(n.to_string()))
            .collect(),
    );
    for (name, expr) in [
        ("ExpressionAlias", alias),
        (
            "ExpressionStatement",
            PegExpr::Call(
                "List".to_string(),
                Box::new(PegExpr::Ref("ExpressionAlias".to_string())),
            ),
        ),
    ] {
        assert!(
            rules.insert(name.to_string(), expr).is_none(),
            "upstream defines `{name}` now; drop this override"
        );
    }
}

/// `StringLiteral (NewlineBefore StringLiteral)*`: a string literal and the
/// continuation lines that concatenate onto it.
fn continued_string() -> PegExpr {
    PegExpr::Seq(vec![
        PegExpr::Ref("StringLiteral".to_string()),
        PegExpr::ZeroOrMore(Box::new(PegExpr::Seq(vec![
            PegExpr::Ref("NewlineBefore".to_string()),
            PegExpr::Ref("StringLiteral".to_string()),
        ]))),
    ])
}

/// Replace every `StringLiteral` reference in `expr` with
/// [`continued_string`]; returns how many were replaced.
fn widen_string_literals(expr: &mut PegExpr) -> usize {
    match expr {
        PegExpr::Ref(name) if name == "StringLiteral" => {
            *expr = continued_string();
            1
        }
        PegExpr::Call(_, arg) => widen_string_literals(arg),
        PegExpr::Seq(items) | PegExpr::Choice(items) => {
            items.iter_mut().map(widen_string_literals).sum()
        }
        PegExpr::Optional(item)
        | PegExpr::ZeroOrMore(item)
        | PegExpr::OneOrMore(item)
        | PegExpr::Not(item)
        | PegExpr::And(item) => widen_string_literals(item),
        PegExpr::Ref(_)
        | PegExpr::Literal(_)
        | PegExpr::Identifier(_)
        | PegExpr::EndOfInput
        | PegExpr::Dummy => 0,
    }
}

fn main() {
    let vendor = manifest_dir().join("vendor/grammar");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed={}", vendor.display());
    for entry in walk(&vendor) {
        println!("cargo:rerun-if-changed={}", entry.display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir().join("vendor/SNAPSHOT").display()
    );

    let kw_dir = vendor.join("keywords");
    let mut keyword_lists = HashMap::new();
    let files = vec![
        ("reserved", "reserved_keyword.list"),
        ("unreserved", "unreserved_keyword.list"),
        ("column_name", "column_name_keyword.list"),
        ("func_name", "func_name_keyword.list"),
        ("type_name", "type_name_keyword.list"),
        // New in the v2.0.0 grammar: the words that may alias a select-list
        // item without `AS` (`BareColLabel <- BareLabelKeyword / Identifier`).
        ("bare_label", "bare_label_keyword.list"),
    ];

    for (cat_name, file_name) in files {
        let p = kw_dir.join(file_name);
        let content = std::fs::read_to_string(&p)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", p.display(), e));
        let list: Vec<String> = content
            .lines()
            .map(|l| l.trim().to_lowercase())
            .filter(|l| !l.is_empty())
            .collect();
        keyword_lists.insert(cat_name.to_string(), list);
    }

    // The five keyword lists OVERLAP (map/struct/tuple/generated are both
    // column-name and func-name; columns/try_cast are both column-name and
    // type-name; 26 words are both func-name and type-name). Emit a BITMASK
    // per word -- a first-match-wins single category would make `map(...)`
    // unparseable.
    let mut kw_flags: std::collections::BTreeMap<String, Vec<&str>> = Default::default();
    for (cat, flag) in [
        ("reserved", "Keyword::RESERVED"),
        ("unreserved", "Keyword::UNRESERVED"),
        ("column_name", "Keyword::COLUMN_NAME"),
        ("type_name", "Keyword::TYPE_NAME"),
        ("func_name", "Keyword::FUNC_NAME"),
        ("bare_label", "Keyword::BARE_LABEL"),
    ] {
        for kw in keyword_lists.get(cat).unwrap() {
            kw_flags.entry(kw.clone()).or_default().push(flag);
        }
    }
    // Bare-label is a property of a keyword, not a category of its own:
    // `Keyword::matches` treats a word with no category bit as an ordinary
    // identifier, so a bare-label word that is in no other list would
    // silently stop being a keyword.
    for (kw, flags) in &kw_flags {
        assert!(
            flags.iter().any(|f| *f != "Keyword::BARE_LABEL"),
            "`{kw}` is in bare_label_keyword.list but in no keyword category list"
        );
    }
    let mut kw_source = String::new();
    kw_source.push_str("use crate::keyword::Keyword;\n\n");
    kw_source.push_str("pub fn classify_keyword(s: &str) -> Keyword {\n");
    kw_source.push_str("    match s.to_ascii_lowercase().as_str() {\n");
    for (kw, flags) in &kw_flags {
        let expr = flags
            .iter()
            .map(|f| format!("{f}.0"))
            .collect::<Vec<_>>()
            .join(" | ");
        kw_source.push_str(&format!("        {kw:?} => Keyword({expr}),\n"));
    }
    kw_source.push_str("        _ => Keyword::NONE,\n");
    kw_source.push_str("    }\n");
    kw_source.push_str("}\n\n");

    let mut parsed_rules = Vec::new();
    let mut macro_defs = HashMap::new();

    let statements_dir = vendor.join("statements");
    for entry in walk(&statements_dir) {
        if entry.extension().and_then(|s| s.to_str()) == Some("gram") {
            let content = std::fs::read_to_string(&entry)
                .unwrap_or_else(|e| panic!("failed to read {}: {}", entry.display(), e));
            let file_rules = parse_gram_file(&content);
            for r in file_rules {
                if let Some(param) = r.param {
                    if macro_defs.insert(r.name.clone(), (param, r.expr)).is_some() {
                        panic!("duplicate macro: {}", r.name);
                    }
                } else {
                    parsed_rules.push(r);
                }
            }
        }
    }

    // Rules DuckDB replaces in C++ at runtime (`AddRuleOverride`); the `.gram`
    // text carries only simplified stubs for them, so the stubs are discarded.
    // Identifier rules become keyword-category matchers (see `keyword.rs`);
    // `Dummy` entries are lexical rules the matcher implements natively.
    let overrides = vec![
        ("Identifier", PegExpr::Identifier(Category::ColumnName)),
        ("PlainIdentifier", PegExpr::Identifier(Category::ColumnName)),
        (
            "QuotedIdentifier",
            PegExpr::Identifier(Category::ColumnName),
        ),
        ("CatalogName", PegExpr::Identifier(Category::ColumnName)),
        ("SchemaName", PegExpr::Identifier(Category::ColumnName)),
        ("ColumnName", PegExpr::Identifier(Category::ColumnName)),
        ("IndexName", PegExpr::Identifier(Category::ColumnName)),
        ("SequenceName", PegExpr::Identifier(Category::ColumnName)),
        ("PragmaName", PegExpr::Identifier(Category::ColumnName)),
        ("SettingName", PegExpr::Identifier(Category::ColumnName)),
        (
            "TableName",
            PegExpr::Choice(vec![
                PegExpr::Identifier(Category::ColumnName),
                PegExpr::Ref("StringLiteral".to_string()),
            ]),
        ),
        ("FunctionName", PegExpr::Identifier(Category::TypeFunc)),
        ("TableFunctionName", PegExpr::Identifier(Category::TypeFunc)),
        ("TypeName", PegExpr::Identifier(Category::TypeName)),
        // `BareColLabel <- BareLabelKeyword / Identifier` in the text; the
        // engine replaces it with an identifier matcher whose only keyword
        // category is bare-label.
        ("BareColLabel", PegExpr::Identifier(Category::BareLabel)),
        ("ReservedIdentifier", PegExpr::Identifier(Category::Any)),
        ("ReservedSchemaName", PegExpr::Identifier(Category::Any)),
        ("ReservedTableName", PegExpr::Identifier(Category::Any)),
        ("ReservedColumnName", PegExpr::Identifier(Category::Any)),
        ("ReservedIndexName", PegExpr::Identifier(Category::Any)),
        ("ReservedFunctionName", PegExpr::Identifier(Category::Any)),
        ("ReservedTypeName", PegExpr::Identifier(Category::Any)),
        ("CopyOptionName", PegExpr::Identifier(Category::Any)),
        ("EndOfInput", PegExpr::EndOfInput),
        ("StringLiteral", PegExpr::Dummy),
        // Zero-width, implemented natively in matcher.rs -- see the
        // LiteralExpression note in apply_structural_overrides.
        ("NewlineBefore", PegExpr::Dummy),
        ("NumberLiteral", PegExpr::Dummy),
        ("OperatorLiteral", PegExpr::Dummy),
        ("AnyOperatorLiteral", PegExpr::Dummy),
        ("ReservedKeyword", PegExpr::Dummy),
        ("UnreservedKeyword", PegExpr::Dummy),
        ("ColumnNameKeyword", PegExpr::Dummy),
        ("FuncNameKeyword", PegExpr::Dummy),
        ("TypeNameKeyword", PegExpr::Dummy),
        ("BareLabelKeyword", PegExpr::Dummy),
    ];

    let mut rules_map = HashMap::new();
    for r in parsed_rules {
        if rules_map.insert(r.name.clone(), r.expr).is_some() {
            panic!("duplicate rule: {}", r.name);
        }
    }

    let override_names: Vec<&str> = overrides.iter().map(|(name, _)| *name).collect();
    for (name, expr) in overrides {
        rules_map.insert(name.to_string(), expr);
    }

    apply_structural_overrides(&mut rules_map);

    let mut expanded_rules = HashMap::new();
    for (name, expr) in &rules_map {
        let expanded = expand_calls(expr, &macro_defs);
        expanded_rules.insert(name.clone(), expanded);
    }

    // A rule nothing references is dead grammar: either upstream left it
    // behind or one of our overrides orphaned it. Either way the build fails,
    // so a grammar refresh cannot carry it in silently.
    let mut referenced = std::collections::HashSet::new();
    for expr in expanded_rules.values() {
        collect_refs(expr, &mut referenced);
    }
    let mut unused: Vec<&String> = expanded_rules
        .keys()
        .filter(|name| {
            // `%`-prefixed entries are directives (`%whitespace`), not rules.
            name.as_str() != "Program"
                && !name.starts_with('%')
                && !override_names.contains(&name.as_str())
                && !referenced.contains(name.as_str())
        })
        .collect();
    unused.sort();
    if !unused.is_empty() {
        panic!("unused grammar rules: {unused:?}");
    }

    let mut sorted_rule_names: Vec<String> = expanded_rules.keys().cloned().collect();
    sorted_rule_names.sort();

    let mut rule_ids = HashMap::new();
    let mut rule_constants = String::new();
    // One constant per grammar rule, over a thousand of them: hidden from the
    // docs so they don't bury the real API, which looks rules up by name.
    for (idx, name) in sorted_rule_names.iter().enumerate() {
        rule_ids.insert(name.clone(), idx as u32);
        let c_name = format!("RULE_{}", camel_to_screaming_snake(name));
        rule_constants.push_str(&format!(
            "#[doc(hidden)]\npub const {}: RuleId = RuleId({});\n",
            c_name, idx
        ));
    }

    let mut compiler = Compiler {
        expressions: Vec::new(),
        rule_ids: rule_ids.clone(),
        shapes: Vec::new(),
    };

    let mut rule_root_exprs = Vec::new();
    for name in &sorted_rule_names {
        let expr = expanded_rules.get(name).unwrap();
        let root_id = compiler.compile_expr(expr);
        rule_root_exprs.push(root_id);
    }

    let mut rules_static = String::new();
    rules_static.push_str("pub struct RuleInfo {\n");
    rules_static.push_str("    pub name: &'static str,\n");
    rules_static.push_str("    pub expr: ExprId,\n");
    rules_static.push_str("}\n\n");
    rules_static.push_str("pub static RULES: &[RuleInfo] = &[\n");
    for (idx, name) in sorted_rule_names.iter().enumerate() {
        let root_id = rule_root_exprs[idx];
        rules_static.push_str(&format!(
            "    RuleInfo {{ name: {:?}, expr: ExprId({}) }},\n",
            name, root_id
        ));
    }
    rules_static.push_str("];\n\n");

    let mut exprs_static = String::new();
    exprs_static.push_str("pub static EXPRESSIONS: &[Expr] = &[\n");
    for (idx, expr) in compiler.expressions.iter().enumerate() {
        exprs_static.push_str(&format!("    /* {} */ {},\n", idx, expr));
    }
    exprs_static.push_str("];\n\n");

    // FIRST sets. Literals are numbered in sorted order of their lowercase
    // text, which is the order the matcher's binary search expects.
    let mut literal_list: Vec<String> = compiler
        .shapes
        .iter()
        .filter_map(|s| match s {
            Shape::Literal(l) => Some(l.to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    literal_list.sort();
    literal_list.dedup();
    assert!(literal_list.len() < u16::MAX as usize);
    let literal_ids: HashMap<String, u16> = literal_list
        .iter()
        .enumerate()
        .map(|(i, l)| (l.clone(), i as u16))
        .collect();
    let rule_roots: HashMap<String, usize> = sorted_rule_names
        .iter()
        .cloned()
        .zip(rule_root_exprs.iter().copied())
        .collect();
    let first = first_sets(&compiler.shapes, &rule_roots, &literal_ids);
    let words = literal_list.len().div_ceil(64);
    // Many expressions share a set: store each once.
    let mut set_ids: HashMap<(u8, Vec<u16>), usize> = HashMap::new();
    let mut sets: Vec<(u8, Vec<u16>)> = Vec::new();
    let mut first_of = Vec::new();
    for (flags, lits) in &first {
        let key = (*flags, lits.iter().copied().collect::<Vec<u16>>());
        let id = *set_ids.entry(key.clone()).or_insert_with(|| {
            sets.push(key);
            sets.len() - 1
        });
        first_of.push(id);
    }
    assert!(sets.len() < u16::MAX as usize);
    let mut first_static = String::new();
    first_static.push_str(&format!(
        "/// Every grammar literal, lowercase and sorted.\npub static LITERALS: &[&str] = &{:?};\n\n",
        literal_list
    ));
    first_static.push_str(&format!(
        "/// 64-bit words in a FIRST set's literal bitmap.\npub const FIRST_WORDS: usize = {words};\n\n"
    ));
    first_static.push_str(
        "/// Each expression's FIRST set, as an index into `FIRST_FLAGS` and `FIRST_LITERALS`.\n",
    );
    first_static.push_str(&format!(
        "pub static FIRST_OF: &[u16] = &{:?};\n\n",
        first_of
    ));
    first_static.push_str("/// Token kinds a set admits, and whether it admits no token at all.\n");
    first_static.push_str(&format!(
        "pub static FIRST_FLAGS: &[u8] = &{:?};\n\n",
        sets.iter().map(|(f, _)| *f).collect::<Vec<u8>>()
    ));
    first_static.push_str("/// The literals a set admits, a bit per entry of `LITERALS`.\n");
    first_static.push_str("pub static FIRST_LITERALS: &[[u64; FIRST_WORDS]] = &[\n");
    for (_, lits) in &sets {
        let mut bits = vec![0u64; words];
        for &l in lits {
            bits[l as usize / 64] |= 1 << (l % 64);
        }
        first_static.push_str(&format!("    {:?},\n", bits));
    }
    first_static.push_str("];\n\n");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("grammar_tables.rs");

    let mut final_content = String::new();
    final_content.push_str("// This file is automatically generated by build.rs. Do not edit.\n\n");
    final_content.push_str(&kw_source);
    final_content.push_str(&rule_constants);
    final_content.push('\n');
    final_content.push_str(&rules_static);
    final_content.push_str(&exprs_static);
    final_content.push_str(&first_static);

    std::fs::write(&dest_path, final_content).unwrap_or_else(|e| {
        panic!(
            "failed to write static tables to {}: {}",
            dest_path.display(),
            e
        )
    });
}

fn camel_to_screaming_snake(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && c.is_uppercase() {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out.replace('%', "")
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap_or_else(|e| panic!("read {}: {e}", d.display())) {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}
