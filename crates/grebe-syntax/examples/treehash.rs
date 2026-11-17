//! Differential helper: reads statements separated by NUL from stdin and
//! prints one line per statement, a hash of its tree (every node's rule and
//! span and every token's owner, in arena order), or `-` if it is rejected.
//! Two builds that print the same lines build the same trees.
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let mut out = String::new();
    for stmt in input.split('\0') {
        if stmt.is_empty() {
            continue;
        }
        match grebe_syntax::matcher::parse(stmt) {
            None => out.push_str("-\n"),
            Some(tree) => {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                for id in tree.walk() {
                    let n = tree.node(id);
                    (id.0, n.rule.0, n.span.start, n.span.end).hash(&mut h);
                    n.parent.map(|p| p.0).hash(&mut h);
                    for c in tree.children(id) {
                        c.0.hash(&mut h);
                    }
                    for t in tree.code_tokens(id) {
                        (t.span.start, t.span.end).hash(&mut h);
                    }
                }
                out.push_str(&format!("{:016x}\n", h.finish()));
            }
        }
    }
    std::io::stdout().write_all(out.as_bytes()).unwrap();
}
