//! The table of known factories: which library calls are factories, and what each
//! property of what they make depends on.

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
}];

/// The rule for `source#export`, if there is one.
pub fn rule(source: &str, export: &str) -> Option<&'static Rule> {
    RULES.iter().find(|rule| rule.names(source, export))
}
