//! CommonJS export tables.
//!
//! `exports.x = …`, `module.exports.x = …` and `module.exports = { … }` are the
//! three ways a CommonJS module writes down what it offers where we can read it. A
//! file that uses one of them has an export table like any other, and is described
//! declaration by declaration rather than as one opaque node.
//!
//! Every other way of touching the table — a computed key, a spread, an
//! `Object.assign`, handing the object to someone else, assigning it from inside a
//! branch — is one we cannot read. That is arranged by omission rather than by a
//! list: this module reports which references to `module` and `exports` reading the
//! table accounted for, and the coarsener flags every one it did not.

use ahash::AHashSet;
use oxc_ast::ast::*;
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
    // `module` and `exports` are the table only where they are the runtime's. A file
    // that declares or imports either one means something else by the name, and an
    // assignment to it is a write to somebody else's object.
    if binds_the_table(program) {
        return Table::default();
    }

    let mut properties: Vec<(Entry, Span)> = Vec::new();
    let mut whole: Vec<(Vec<Entry>, Span)> = Vec::new();

    for (index, statement) in program.body.iter().enumerate() {
        let Some(assignment) = assignment(statement) else {
            continue;
        };
        let Some(target) = target(&assignment.left) else {
            continue;
        };

        match target {
            Target::Property(name, span) => properties.push((
                Entry {
                    name,
                    statement: index,
                },
                span,
            )),
            Target::Whole(span) => {
                // Only an object literal says what the table holds. Anything else —
                // a class, a function, a call — is a value we cannot take names from.
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
                whole.push((entries, span));
            }
        }
    }

    // A file writes its table one way or the other. Mixing a whole-table assignment
    // with anything else, or writing two of them, means the order they run in decides
    // what survives — and a table we would have to reason about the order of is one
    // we cannot read.
    let table = match (whole.len(), properties.is_empty()) {
        (0, _) => Table {
            accounted: properties.iter().map(|(_, span)| *span).collect(),
            entries: properties.into_iter().map(|(entry, _)| entry).collect(),
        },
        (1, true) => {
            let (entries, span) = whole.pop().expect("one whole-table assignment");
            Table {
                entries,
                accounted: [span].into_iter().collect(),
            }
        }
        _ => Table::default(),
    };

    // One name assigned twice is the same problem in miniature: which assignment
    // survives is a question about order.
    let mut seen: AHashSet<&str> = AHashSet::default();
    if table
        .entries
        .iter()
        .any(|entry| !seen.insert(entry.name.as_str()))
    {
        return Table::default();
    }

    table
}

/// Whether the file binds `module` or `exports` itself, at the scope an assignment
/// to one would resolve in.
fn binds_the_table(program: &Program<'_>) -> bool {
    let mut bound = false;
    let mut note = |name: &str| bound |= name == "module" || name == "exports";

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

/// The plain `=` assignment a top-level statement is, if it is one.
fn assignment<'a>(statement: &'a Statement<'a>) -> Option<&'a AssignmentExpression<'a>> {
    let Statement::ExpressionStatement(statement) = statement else {
        return None;
    };
    let Expression::AssignmentExpression(assignment) = &statement.expression else {
        return None;
    };
    (assignment.operator == AssignmentOperator::Assign).then_some(assignment.as_ref())
}

/// What the left-hand side of a top-level assignment names, with the span of the
/// member expression that reading it accounts for.
enum Target {
    /// `exports.x`, `module.exports.x`
    Property(String, Span),
    /// `module.exports`, which replaces the table rather than adding to it.
    Whole(Span),
}

fn target(left: &AssignmentTarget<'_>) -> Option<Target> {
    let AssignmentTarget::StaticMemberExpression(member) = left else {
        return None;
    };

    match &member.object {
        Expression::Identifier(object) if object.name == "exports" => Some(Target::Property(
            member.property.name.to_string(),
            span_of(member.span()),
        )),
        Expression::Identifier(object)
            if object.name == "module" && member.property.name == "exports" =>
        {
            Some(Target::Whole(span_of(member.span())))
        }
        // `module.exports.x`: the inner member is the one naming the table, and so
        // the one the coarsener would flag.
        Expression::StaticMemberExpression(inner) => match &inner.object {
            Expression::Identifier(object)
                if object.name == "module" && inner.property.name == "exports" =>
            {
                Some(Target::Property(
                    member.property.name.to_string(),
                    span_of(inner.span()),
                ))
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
    fn a_file_that_binds_the_name_itself_means_something_else_by_it() {
        // `exports` here is somebody else's object, and writing to it is a side
        // effect on them rather than an export of ours.
        assert!(names("import { exports } from './x'; exports.a = 1;").is_empty());
        assert!(names("const exports = target; exports.a = 1;").is_empty());
        assert!(names("function module() {} module.exports = { a: 1 };").is_empty());
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
