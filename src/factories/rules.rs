//! The table of known factories: which library calls are factories, and what each
//! property of what they make depends on.

use crate::module::Step;

/// One factory, and what its result's properties depend on.
#[derive(Debug, PartialEq, Eq)]
pub struct Rule {
    /// The module specifiers it is imported from, written as an import writes them.
    pub sources: &'static [&'static str],
    /// The name those modules export it under.
    pub export: &'static str,
    /// Properties of its result read on their own, each with the arguments, by
    /// position, it depends on.
    pub members: &'static [(&'static str, &'static [usize])],
    /// What calling it runs when the value is created, which is what module
    /// initialisation reaches.
    pub creation: Creation,
    /// Forms that read the factory and return it unchanged, so that calling what
    /// they return is calling the factory.
    pub identity: &'static [Identity],
}

/// A form that reads a factory and returns the factory itself, as a rule declares
/// for its own factory and no other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// `.name()`, called with no arguments, whatever its type arguments: RTK's
    /// `createAsyncThunk.withTypes<T>()`, which only types the factory.
    Method(&'static str),
}

impl Identity {
    /// `path` without this form at its end, if it ends with it.
    fn strip<'p>(&self, path: &'p [Step]) -> Option<&'p [Step]> {
        match (self, path) {
            (Identity::Method(method), [rest @ .., Step::Prop(name), Step::Call])
                if name == method =>
            {
                Some(rest)
            }
            _ => None,
        }
    }
}

/// What creating a factory's value reaches, and so what module initialisation
/// depends on when a module creates one at the top level.
///
/// A factory that calls one of its arguments while creating the value would be a
/// variant of its own here, naming those arguments, so that initialisation reaches
/// them along with the frame and still leaves the rest alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Creation {
    /// Creating the value calls the factory and nothing it is given. Initialisation
    /// reaches the call's frame, the callee and the call around the arguments, and
    /// none of the arguments.
    Frame,
    /// Creating the value may run anything the call names, so initialisation
    /// reaches the whole declaration, as it does for a call of no known factory.
    Whole,
}

impl Rule {
    /// The arguments `name` depends on, if it is a property the rule lists.
    pub fn member(&self, name: &str) -> Option<&'static [usize]> {
        self.members
            .iter()
            .find(|(member, _)| *member == name)
            .map(|(_, args)| *args)
    }

    /// Every property the rule lists.
    pub fn member_names(&self) -> impl Iterator<Item = &'static str> {
        self.members.iter().map(|(name, _)| *name)
    }

    /// Whether `source#export` names this factory.
    pub fn names(&self, source: &str, export: &str) -> bool {
        self.export == export && self.sources.contains(&source)
    }

    /// What is left of `path`, read off the factory, once every identity form this
    /// rule declares is taken off its end. An empty path is the factory itself.
    ///
    /// Only this rule's forms are taken: the same call on another factory may
    /// return something else entirely.
    pub fn strip_identity<'p>(&self, mut path: &'p [Step]) -> &'p [Step] {
        while let Some(rest) = self.identity.iter().find_map(|form| form.strip(path)) {
            path = rest;
        }
        path
    }
}

/// Whether some rule declares `.name()` an identity form, which is what the
/// parse-time half asks before keeping such a call as a step: it cannot tell which
/// rule, if any, the callee will turn out to be.
pub fn is_identity_method(name: &str) -> bool {
    RULES.iter().any(|rule| {
        rule.identity
            .iter()
            .any(|form| matches!(form, Identity::Method(method) if *method == name))
    })
}

/// `createAsyncThunk(type, payloadCreator, options)` returns a thunk action creator
/// with `pending`, `fulfilled` and `rejected` action creators, a `settled` matcher,
/// and its `typePrefix`, built from `type`. `rejected` also serialises the error it
/// is given with `options.serializeError`, so it reads the options too. The payload
/// creator only runs when the thunk is dispatched, and the rest of the options with
/// it.
pub const RULES: &[Rule] = &[Rule {
    sources: &["@reduxjs/toolkit", "@reduxjs/toolkit/react"],
    export: "createAsyncThunk",
    members: &[
        ("pending", &[0]),
        ("fulfilled", &[0]),
        ("rejected", &[0, 2]),
        ("settled", &[0]),
        ("typePrefix", &[0]),
    ],
    creation: Creation::Frame,
    identity: &[Identity::Method("withTypes")],
}];

/// The rule for `source#export`, if there is one.
pub fn rule(source: &str, export: &str) -> Option<&'static Rule> {
    RULES.iter().find(|rule| rule.names(source, export))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop(name: &str) -> Step {
        Step::Prop(name.to_string())
    }

    #[test]
    fn a_rule_takes_its_own_identity_forms_off_the_end_of_a_path() {
        let rtk = rule("@reduxjs/toolkit", "createAsyncThunk").unwrap();
        let typed = [prop("withTypes"), Step::Call];
        assert!(rtk.strip_identity(&typed).is_empty());
        let twice = [prop("withTypes"), Step::Call, prop("withTypes"), Step::Call];
        assert!(rtk.strip_identity(&twice).is_empty());

        // Read without calling it, or called with something read off the result,
        // it is not the factory.
        for path in [
            vec![prop("withTypes")],
            vec![prop("withTypes"), Step::Call, prop("fulfilled")],
            vec![prop("other"), Step::Call],
            vec![Step::Call],
        ] {
            assert_eq!(rtk.strip_identity(&path), path.as_slice(), "{path:?}");
        }
    }

    #[test]
    fn an_identity_form_is_the_rule_that_declares_it_and_no_other() {
        let other = Rule {
            sources: &["library"],
            export: "make",
            members: &[],
            creation: Creation::Frame,
            identity: &[],
        };
        let typed = [prop("withTypes"), Step::Call];
        assert_eq!(other.strip_identity(&typed), typed.as_slice());

        assert!(is_identity_method("withTypes"));
        assert!(!is_identity_method("other"));
    }
}
