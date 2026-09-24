//! CommonJS export tables.
//!
//! A CommonJS module writes down what it offers as assignment, and these are the
//! spellings that say it plainly enough to read:
//!
//! ```js
//! exports.parse = …                    // one name
//! module.exports.parse = …             // the same name, said the long way
//! module.exports = { parse, format }   // the whole table at once
//! exports.parse = exports.read = …     // one value, two names
//! exports.parse = void 0;              // a compiler promising a name to come
//! var _default = (exports.default = …) // a compiler's `export default`
//! Object.defineProperty(exports, "parse", { get() { … } })  // a re-export
//! ```
//!
//! The last four are what a compiler emits for an ES module, which is most of the
//! CommonJS anybody reads today. A file written in any of them has an export table
//! like any other, and is described declaration by declaration rather than as one
//! opaque node.
//!
//! Every other way of touching the table — a computed key, a spread, an
//! `Object.assign`, handing the object to someone else, assigning it from inside a
//! branch — is one we cannot read. That is arranged by omission rather than by a
//! list: this module reports which mentions of `module` and `exports` reading the
//! table accounted for, and the coarsener flags every one it did not.

use ahash::AHashSet;
use oxc_ast::ast::*;
use oxc_ast_visit::Visit;
use oxc_span::GetSpan;

use super::Span;
use super::parse::span_of;

/// A name a module puts in its CommonJS export table.
pub(crate) struct Entry {
    pub name: String,
    /// Index of the top-level statement that assigns it.
    pub statement: usize,
}

/// A file's CommonJS export table, and the references to `module` and `exports`
/// that reading it accounted for.
///
/// Empty for a file that writes no table, and for one whose table we cannot read.
/// The two need no telling apart: a file with nothing to account for has nothing
/// left over either.
#[derive(Default)]
pub(crate) struct Table {
    pub entries: Vec<Entry>,
    pub accounted: AHashSet<Span>,
}

impl Table {
    /// The name this table exports through the given top-level statement, if any.
    pub fn names_at(&self, statement: usize) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(move |entry| entry.statement == statement)
            .map(|entry| entry.name.as_str())
    }
}

/// The name a declaration standing for a CommonJS export goes by.
///
/// A dot keeps it clear of every binding the file could declare, so the export and
/// a local of the same name stay two different things.
pub(crate) fn decl_name(export: &str) -> String {
    format!("exports.{export}")
}

pub(crate) fn table(program: &Program<'_>) -> Table {
    read(program).unwrap_or_default()
}

/// The file's export table, or the reason there is none to be read.
///
/// The reason goes nowhere, as the coarsener's does: it is here so that each way of
/// giving up is named where it happens, and so that measuring which of them fires on
/// a real tree costs a `println!` rather than a rewrite.
fn read(program: &Program<'_>) -> Result<Table, &'static str> {
    // These names are the runtime's or this reads nothing: an assignment to an
    // `exports` the file declared is a write to somebody else's object, and a
    // `defineProperty` on an `Object` it declared is somebody else's function.
    if binds_a_name_we_trust(program) {
        return Err("the file binds a name this reads as the runtime's");
    }

    let mut properties: Vec<(Entry, Vec<Span>)> = Vec::new();
    let mut whole: Vec<(Vec<Entry>, Vec<Span>)> = Vec::new();
    let mut promises: Vec<Promise> = Vec::new();

    for (index, statement) in program.body.iter().enumerate() {
        // `Object.defineProperty(exports, "x", { get() { … } })`, which is how a
        // compiler writes a re-export.
        if let Some((name, spans)) = defined_property(statement) {
            properties.push((
                Entry {
                    name,
                    statement: index,
                },
                spans,
            ));
            continue;
        }

        for assignment in assignments(statement) {
            let Some(chain) = chain(assignment) else {
                continue;
            };

            match chain {
                // `module.exports = { … }`, the whole table at once. Only an object
                // literal says what it holds; anything else — a class, a function, a
                // call — is a value we cannot take names from.
                Chain::Whole(spans) => {
                    let Some(names) = object_names(&assignment.right) else {
                        continue;
                    };
                    let entries = names
                        .into_iter()
                        .map(|name| Entry {
                            name,
                            statement: index,
                        })
                        .collect();
                    whole.push((entries, spans));
                }
                // `exports.a = …`, and `exports.a = exports.b = …`, which says the
                // same of every name in it.
                Chain::Names(names, spans) => {
                    for (name, spans) in names.into_iter().zip(spans) {
                        properties.push((
                            Entry {
                                name,
                                statement: index,
                            },
                            spans,
                        ));
                    }
                }
                Chain::Promise(names, spans) => promises.push(Promise {
                    names,
                    spans: spans.into_iter().flatten().collect(),
                    statement: index,
                }),
            }
        }
    }

    // A file writes its table one way or the other. Mixing a whole-table assignment
    // with anything else, or writing two of them, means the order they run in decides
    // what survives — and a table we would have to reason about the order of is one
    // we cannot read.
    let mut table = match (whole.len(), properties.is_empty()) {
        (0, _) => Table {
            accounted: properties
                .iter()
                .flat_map(|(_, spans)| spans)
                .copied()
                .collect(),
            entries: properties.into_iter().map(|(entry, _)| entry).collect(),
        },
        (1, true) => {
            let (entries, spans) = whole.pop().expect("one whole-table assignment");
            Table {
                entries,
                accounted: spans.into_iter().collect(),
            }
        }
        _ => return Err("the whole table and its properties are both assigned"),
    };

    // One name assigned twice is the same problem in miniature: which assignment
    // survives is a question about order.
    let mut seen: AHashSet<&str> = AHashSet::default();
    if table
        .entries
        .iter()
        .any(|entry| !seen.insert(entry.name.as_str()))
    {
        return Err("a name is assigned twice");
    }

    // A promise is the file stating its own table, which makes it a free check on
    // ours. The coarsener would catch these anyway — a name assigned some way we do
    // not read is a mention we did not account for — so this is belt and braces, and
    // says in one place what the rest of the module only implies.
    for promise in &promises {
        for name in &promise.names {
            // A name promised and never assigned is one the file exports some way we
            // did not read, and the table we have is not the file's.
            let Some(entry) = table.entries.iter().find(|entry| &entry.name == name) else {
                return Err("a promised name is never assigned");
            };
            // The promise comes before the table is filled in. An assignment it
            // overwrote is one whose value never reached anybody.
            if entry.statement < promise.statement {
                return Err("a promise overwrites an assignment above it");
            }
        }
    }

    // A CommonJS method is called with the export object as `this`. Reading a
    // sibling through that receiver bypasses lexical symbol edges, so a table in
    // a file using `this` cannot safely be split into independent exports yet.
    if !table.entries.is_empty() {
        let mut receiver = ReceiverUse(false);
        receiver.visit_program(program);
        if receiver.0 {
            return Err("this may refer to the export table");
        }
    }

    table
        .accounted
        .extend(promises.into_iter().flat_map(|promise| promise.spans));
    Ok(table)
}

struct ReceiverUse(bool);

impl<'a> Visit<'a> for ReceiverUse {
    fn visit_this_expression(&mut self, _: &ThisExpression) {
        self.0 = true;
    }
}

/// Whether the file binds any of the names this reader takes to be the runtime's, at
/// the scope a top-level mention of one would resolve in.
///
/// `module` and `exports` are the table; `Object` is the one whose `defineProperty`
/// a re-export is read through. A file that declares or imports any of them means
/// something of its own by the name, and nothing here is about the runtime any more.
fn binds_a_name_we_trust(program: &Program<'_>) -> bool {
    let mut bound = false;
    let mut note = |name: &str| bound |= name == "module" || name == "exports" || name == "Object";

    for statement in &program.body {
        match statement {
            Statement::ImportDeclaration(import) => {
                for specifier in import.specifiers.iter().flatten() {
                    note(&specifier.local().name);
                }
            }
            Statement::ExportDeclaration(export) => {
                note_declaration(&export.declaration, &mut note)
            }
            statement => {
                if let Some(declaration) = statement.as_declaration() {
                    note_declaration(declaration, &mut note);
                }
            }
        }
    }

    bound
}

fn note_declaration(declaration: &Declaration<'_>, note: &mut impl FnMut(&str)) {
    match declaration {
        Declaration::VariableDeclaration(variable) => {
            for declarator in &variable.declarations {
                for name in declarator.id.get_binding_identifiers() {
                    note(&name.name);
                }
            }
        }
        Declaration::FunctionDeclaration(function) => {
            if let Some(id) = &function.id {
                note(&id.name);
            }
        }
        Declaration::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                note(&id.name);
            }
        }
        _ => {}
    }
}

/// A compiler's promise about the table it is about to fill in.
struct Promise {
    names: Vec<String>,
    spans: Vec<Span>,
    statement: usize,
}

/// What a top-level assignment statement says about the table. Each name comes with
/// the mentions of `module` and `exports` that reading it accounts for.
enum Chain {
    /// `module.exports = …`, which replaces the table rather than adding to it.
    Whole(Vec<Span>),
    /// `exports.a = …`, and `exports.a = exports.b = …`, which gives every name in
    /// the chain the same value.
    Names(Vec<String>, Vec<Vec<Span>>),
    /// `exports.a = exports.b = void 0;`, which every compiler emits ahead of the
    /// assignments that mean something, so that the shape of the table is settled
    /// before any of it is read.
    ///
    /// It holds no value worth following — that is the point of it — so it
    /// contributes no entry, only the names it promises. What makes it safe to pass
    /// over is that every one of those names has to turn up in the table anyway.
    Promise(Vec<String>, Vec<Vec<Span>>),
}

/// The chain of table names a top-level assignment writes to, and what it writes.
fn chain(assignment: &AssignmentExpression<'_>) -> Option<Chain> {
    let mut names = Vec::new();
    let mut spans = Vec::new();
    let mut current = assignment;

    loop {
        if current.operator != AssignmentOperator::Assign {
            return None;
        }

        let mut accounted = Vec::new();
        match target(&current.left, &mut accounted)? {
            // The whole table can only be the outermost of a chain: `module.exports`
            // inside one is a value being read, not a table being replaced.
            Target::Whole if names.is_empty() => return Some(Chain::Whole(accounted)),
            Target::Whole => return None,
            Target::Property(name) => {
                names.push(name);
                spans.push(accounted);
            }
        }

        match &current.right {
            Expression::AssignmentExpression(next) => current = next,
            right if is_nothing_yet(right) => return Some(Chain::Promise(names, spans)),
            _ => return Some(Chain::Names(names, spans)),
        }
    }
}

/// `Object.defineProperty(exports, "x", { enumerable: true, get() { … } })`: the
/// name, and the mention of `exports` that reading it accounts for.
///
/// A compiler writes `export { x } from "./y"` this way, so the statement is a
/// declaration of `x` whose value is whatever the descriptor returns. Only a
/// descriptor that runs nothing on the way past is read — defining an accessor does
/// not call it — which leaves `{ value: compute() }` for the coarsener.
fn defined_property(statement: &Statement<'_>) -> Option<(String, Vec<Span>)> {
    let Statement::ExpressionStatement(statement) = statement else {
        return None;
    };
    let Expression::CallExpression(call) = &statement.expression else {
        return None;
    };

    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return None;
    };
    let Expression::Identifier(object) = &callee.object else {
        return None;
    };
    if object.name != "Object" || callee.property.name != "defineProperty" {
        return None;
    }

    let [table, name, descriptor] = &call.arguments[..] else {
        return None;
    };
    let Argument::Identifier(table) = table else {
        return None;
    };
    if table.name != "exports" {
        return None;
    }
    let Argument::StringLiteral(name) = name else {
        return None;
    };
    let Argument::ObjectExpression(descriptor) = descriptor else {
        return None;
    };

    for property in &descriptor.properties {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            return None;
        };
        let key = property.key.static_name()?;
        let inert = match key.as_ref() {
            // Defining an accessor stores the function; it does not call it.
            "get" | "set" => matches!(
                property.value,
                Expression::FunctionExpression(_) | Expression::ArrowFunctionExpression(_)
            ),
            // A flag, and a value that is only a value.
            "enumerable" | "configurable" | "writable" | "value" => property.value.is_literal(),
            _ => false,
        };
        if !inert {
            return None;
        }
    }

    Some((name.value.to_string(), vec![span_of(table.span)]))
}

/// `void 0` and `undefined`: a value written down as "nothing yet". `void` of
/// anything but a literal runs it, and is a value like any other.
fn is_nothing_yet(expr: &Expression<'_>) -> bool {
    match expr {
        Expression::Identifier(identifier) => identifier.name == "undefined",
        Expression::UnaryExpression(unary) => {
            unary.operator == UnaryOperator::Void
                && matches!(unary.argument, Expression::NumericLiteral(_))
        }
        _ => false,
    }
}

/// The value an expression gives the export table, if giving it one is all it does.
///
/// `exports.x = value` puts a value under a name; it is the export, not an effect on
/// anybody else, so what decides whether the statement runs anything is the value.
pub(crate) fn assigned_value<'a>(expression: &'a Expression<'a>) -> Option<&'a Expression<'a>> {
    let Expression::AssignmentExpression(assignment) = expression.get_inner_expression() else {
        return None;
    };
    // Every name it writes to has to be the table's; anything else is a write we are
    // not entitled to overlook.
    chain(assignment)?;

    let mut current = assignment.as_ref();
    loop {
        match current.right.get_inner_expression() {
            Expression::AssignmentExpression(next) => current = next.as_ref(),
            value => return Some(value),
        }
    }
}

/// The assignments a top-level statement makes.
///
/// Usually the statement is the assignment. A compiler also writes the table from
/// inside a declaration — `var _default = (exports.default = …)` is how Babel writes
/// `export default` — and that assignment is the same assignment wherever it sits.
fn assignments<'a>(statement: &'a Statement<'a>) -> Vec<&'a AssignmentExpression<'a>> {
    let expressions: Vec<&Expression<'a>> = match statement {
        Statement::ExpressionStatement(statement) => vec![&statement.expression],
        Statement::VariableDeclaration(declaration) => declaration
            .declarations
            .iter()
            .filter_map(|declarator| declarator.init.as_ref())
            .collect(),
        _ => return Vec::new(),
    };

    expressions
        .into_iter()
        .filter_map(|expression| match expression.get_inner_expression() {
            Expression::AssignmentExpression(assignment) => Some(assignment.as_ref()),
            _ => None,
        })
        .collect()
}

/// What the left-hand side of a top-level assignment names.
enum Target {
    /// `exports.x`, `module.exports.x`
    Property(String),
    /// `module.exports`, which replaces the table rather than adding to it.
    Whole,
}

/// The name, pushing onto `accounted` every mention of `module` and `exports` that
/// reading it speaks for — the member expressions and the identifier under them.
fn target(left: &AssignmentTarget<'_>, accounted: &mut Vec<Span>) -> Option<Target> {
    let AssignmentTarget::StaticMemberExpression(member) = left else {
        return None;
    };

    match &member.object {
        Expression::Identifier(object) if object.name == "exports" => {
            accounted.extend([span_of(member.span()), span_of(object.span)]);
            Some(Target::Property(member.property.name.to_string()))
        }
        Expression::Identifier(object)
            if object.name == "module" && member.property.name == "exports" =>
        {
            accounted.extend([span_of(member.span()), span_of(object.span)]);
            Some(Target::Whole)
        }
        // `module.exports.x`: the inner member is the one naming the table, and so
        // the one the coarsener would flag.
        Expression::StaticMemberExpression(inner) => match &inner.object {
            Expression::Identifier(object)
                if object.name == "module" && inner.property.name == "exports" =>
            {
                accounted.extend([span_of(inner.span()), span_of(object.span)]);
                Some(Target::Property(member.property.name.to_string()))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The names an object literal holds, or `None` if it holds any we cannot list.
fn object_names(right: &Expression<'_>) -> Option<Vec<String>> {
    let Expression::ObjectExpression(object) = right else {
        return None;
    };

    let mut names = Vec::with_capacity(object.properties.len());
    for property in &object.properties {
        // A spread brings in whatever the other object holds.
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            return None;
        };
        if property.computed {
            return None;
        }
        names.push(property.key.static_name()?.to_string());
    }
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    /// The names a source's export table holds, in the order it writes them.
    fn names(source: &str) -> Vec<String> {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, SourceType::default()).parse();
        assert!(parsed.diagnostics.is_empty(), "fixture should parse");
        table(&parsed.program)
            .entries
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    }

    #[test]
    fn both_spellings_of_a_property_name_the_same_table() {
        assert_eq!(
            names("exports.a = 1; module.exports.b = 2;"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn an_object_literal_is_the_table() {
        assert_eq!(
            names("module.exports = { a, b: 2, 'c': 3 };"),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn a_table_written_two_ways_cannot_be_read() {
        // Which assignment survives is a question about the order they run in.
        assert!(names("exports.a = 1; module.exports = { b: 2 };").is_empty());
        assert!(names("module.exports = { a: 1 }; module.exports = { b: 2 };").is_empty());
        assert!(names("exports.a = 1; exports.a = 2;").is_empty());
    }

    #[test]
    fn a_name_that_cannot_be_listed_cannot_be_read() {
        assert!(names("module.exports[key] = 1;").is_empty());
        assert!(names("module.exports = { ...other };").is_empty());
        assert!(names("module.exports = { [key]: 1 };").is_empty());
    }

    #[test]
    fn a_table_that_is_not_an_object_cannot_be_read() {
        // Whatever `Widget` holds is not written down here.
        assert!(names("module.exports = Widget;").is_empty());
        assert!(names("module.exports = build();").is_empty());
    }

    #[test]
    fn only_a_top_level_assignment_is_the_table() {
        // Assigned from inside a branch, whether it happens at all is a question
        // about what runs, which is not one this reads.
        assert!(names("if (flag) { exports.a = 1; }").is_empty());
        assert!(names("function install() { exports.a = 1; }").is_empty());
    }

    #[test]
    fn a_compilers_promise_contributes_no_name_of_its_own() {
        // The prelude every compiler emits, and then the table it promised.
        assert_eq!(
            names("exports.b = exports.a = void 0; exports.a = 1; exports.b = 2;"),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            names("exports.a = undefined; exports.a = 1;"),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn a_promise_the_table_does_not_keep_cannot_be_read() {
        // `b` is exported some way this does not read — a getter, a branch, a helper
        // handed the table — so the table we have is not the file's.
        assert!(names("exports.b = exports.a = void 0; exports.a = 1;").is_empty());
        // The promise comes first, so an assignment before it is one it overwrote.
        assert!(names("exports.a = 1; exports.a = void 0;").is_empty());
        // A name the promise says nothing about is not its business, though: the
        // `__esModule` marker sits above the prelude in every compiler's output.
        assert_eq!(
            names("exports.__esModule = true; exports.a = void 0; exports.a = 1;"),
            vec!["__esModule".to_string(), "a".to_string()]
        );
    }

    #[test]
    fn only_a_literal_is_nothing_yet() {
        // `void build()` runs the call; the statement is an assignment like any
        // other, and assigning `a` twice coarsens.
        assert!(names("exports.a = void build(); exports.a = 1;").is_empty());
    }

    #[test]
    fn a_file_that_binds_the_name_itself_means_something_else_by_it() {
        // `exports` here is somebody else's object, and writing to it is a side
        // effect on them rather than an export of ours.
        assert!(names("import { exports } from './x'; exports.a = 1;").is_empty());
        assert!(names("const exports = target; exports.a = 1;").is_empty());
        assert!(names("function module() {} module.exports = { a: 1 };").is_empty());
        // `Object` too: a re-export is read through its `defineProperty`.
        assert!(
            names("const Object = shim; Object.defineProperty(exports, 'a', { value: 1 });")
                .is_empty()
        );
    }

    #[test]
    fn a_file_with_no_table_has_nothing_to_account_for() {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, "const a = 1;", SourceType::default()).parse();
        let table = table(&parsed.program);
        assert!(table.entries.is_empty());
        assert!(table.accounted.is_empty());
    }
}
