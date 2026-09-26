//! Globals whose calls have no side effects.
//!
//! A side effect here means a change other code in the app could read back and
//! behave differently for. That is what leaving a call out of module initialisation
//! needs, and it is all it needs: the call does not have to give the same answer
//! every time, or keep to itself. So `Date.now()` and `Math.random()` are here —
//! each reads the clock or a random source, and changes nothing the app reads — and
//! so is `console.log()`, which writes somewhere the app never reads from. A
//! declaration holding a result is reached by whatever reads it, like any other.
//!
//! The lists are taken from `oxc_ecmascript`'s side-effect analysis, which keeps
//! them private to itself; its own tables are ported in turn from Rolldown, Rollup
//! and closure-compiler. That analysis assumes `toString` and `valueOf` never run
//! code of their own. This one does not, so each function here is listed with the
//! conversion it applies to its arguments, and a caller may accept a call only where
//! that conversion cannot reach user code or throw.
//!
//! Entries the source lists but a plain literal argument can still make throw are
//! left out: `decodeURI("%")`, `encodeURI` of a lone surrogate, `escape`,
//! `String.fromCodePoint(-1)`, and `WeakMap` or `WeakSet` given entries, which
//! throw on a primitive key. So is `Symbol.for`, which does have an effect: it adds
//! its key to the global symbol registry every module shares.
//!
//! Every name is the global's only if the reference has no binding in the file,
//! which the caller checks. The environment's own globals are assumed to be as the
//! language defines them, which is what the source assumes too.

/// What a global function does to its arguments before it looks at them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Conversion {
    /// Nothing: any argument is read as it is.
    None,
    /// `ToString`, which runs an object's `toString` and throws on a symbol.
    ToString,
    /// `ToNumber`, which runs an object's `valueOf` and throws on a BigInt or a
    /// symbol.
    ToNumber,
    /// Shown as they are, the way the console shows a value, except where the first
    /// of several is a format string: `%s` and the rest convert what follows it.
    Shown,
}

/// What a pure global function returns, for a caller that needs to know whether
/// the result can itself be converted safely.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Returns {
    /// A string, number or boolean.
    Primitive,
    /// Anything else, which a conversion may not be applied to.
    Other,
}

/// A call to a global function, `name(...)`, that has no side effects.
#[rustfmt::skip]
pub(crate) fn function(name: &str) -> Option<(Conversion, Returns)> {
    use Conversion::*;
    use Returns::*;
    Some(match name {
        "isFinite" | "isNaN" => (ToNumber, Primitive),
        "parseFloat" => (ToString, Primitive),
        // The string is converted to a string, but the radix goes through
        // `ToInt32`, which throws on a BigInt. One conversion covers every argument,
        // so it is the stricter of the two.
        "parseInt" => (ToNumber, Primitive),
        // `String(symbol)` describes the symbol rather than converting it.
        "String" => (ToString, Primitive),
        "Boolean" => (None, Primitive),
        "Object" => (None, Other),
        // `Date()` called as a function ignores its arguments.
        "Date" => (None, Primitive),
        _ => return Option::None,
    })
}

/// A call to a method of a global namespace or constructor, `object.method(...)`,
/// that has no side effects.
#[rustfmt::skip]
pub(crate) fn method(object: &str, method: &str) -> Option<(Conversion, Returns)> {
    use Conversion::*;
    use Returns::*;
    Some(match (object, method) {
        ("Math", method) if is_math_method(method) => (ToNumber, Primitive),
        ("Number", "isFinite" | "isInteger" | "isNaN" | "isSafeInteger") => (None, Primitive),
        ("Number", "parseFloat") => (ToString, Primitive),
        ("Number", "parseInt") => (ToNumber, Primitive),
        ("Array", "isArray") => (None, Primitive),
        ("Array", "of") => (None, Other),
        ("ArrayBuffer", "isView") => (None, Primitive),
        ("Object", "is") => (None, Primitive),
        ("Date", "now") => (None, Primitive),
        ("Date", "parse") => (ToString, Primitive),
        ("Date", "UTC") => (ToNumber, Primitive),
        ("String", "fromCharCode") => (ToNumber, Primitive),
        // Writes to the console, which nothing in the app reads back.
        ("console", method) if is_console_method(method) => (Shown, Primitive),
        _ => return Option::None,
    })
}

/// A constructor, `new name(...)`, that has no side effects.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Constructor {
    /// Arguments are converted, as for a function call.
    Converting(Conversion),
    /// `Set`: iterates an array literal, if given one.
    Set,
    /// `Map`: iterates an array literal of array-literal entries, if given one.
    Map,
    /// `WeakMap` and `WeakSet`: nothing but `null` or no argument, since an entry
    /// with a primitive key throws.
    Empty,
}

#[rustfmt::skip]
pub(crate) fn constructor(name: &str) -> Option<Constructor> {
    use Constructor::*;
    Some(match name {
        "Set" => Set,
        "Map" => Map,
        "WeakMap" | "WeakSet" => Empty,
        "Object" | "Boolean" => Converting(Conversion::None),
        "String" | "Error" | "EvalError" | "RangeError" | "ReferenceError" | "SyntaxError"
        | "TypeError" | "URIError" => Converting(Conversion::ToString),
        "Date" | "Number" => Converting(Conversion::ToNumber),
        _ => return Option::None,
    })
}

/// A property of a global namespace that holds a number: `Math.PI`,
/// `Number.MAX_SAFE_INTEGER`.
#[rustfmt::skip]
pub(crate) fn constant(object: &str, property: &str) -> bool {
    match object {
        "Math" => matches!(property, "E" | "LN10" | "LN2" | "LOG10E" | "LOG2E" | "PI" | "SQRT1_2" | "SQRT2"),
        "Number" => matches!(property,
            "POSITIVE_INFINITY" | "NEGATIVE_INFINITY" | "EPSILON" | "NaN"
            | "MAX_VALUE" | "MIN_VALUE" | "MAX_SAFE_INTEGER" | "MIN_SAFE_INTEGER"),
        _ => false,
    }
}

/// A global that holds a primitive: `undefined`, `NaN`, `Infinity`.
pub(crate) fn primitive_global(name: &str) -> bool {
    matches!(name, "undefined" | "NaN" | "Infinity")
}

#[rustfmt::skip]
fn is_console_method(method: &str) -> bool {
    matches!(method,
        "assert" | "count" | "countReset" | "debug" | "dir" | "dirxml" | "error"
        | "group" | "groupCollapsed" | "groupEnd" | "info" | "log" | "table" | "time"
        | "timeEnd" | "timeLog" | "trace" | "warn"
    )
}

#[rustfmt::skip]
fn is_math_method(method: &str) -> bool {
    matches!(method,
        "abs" | "acos" | "acosh" | "asin" | "asinh" | "atan" | "atan2" | "atanh"
        | "cbrt" | "ceil" | "clz32" | "cos" | "cosh" | "exp" | "expm1" | "floor"
        | "fround" | "hypot" | "imul" | "log" | "log10" | "log1p" | "log2" | "max"
        | "min" | "pow" | "random" | "round" | "sign" | "sin" | "sinh" | "sqrt"
        | "tan" | "tanh" | "trunc"
    )
}
