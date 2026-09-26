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
//! and closure-compiler. Like that analysis, this takes `toString` and `valueOf` to
//! have no side effects.
//!
//! What matters beyond that is throwing, and only in the module body: a throw there
//! stops the module loading, and every importer with it. A throw from a helper's
//! body does not make the helper one with side effects. So each function is listed
//! with the conversion it applies to its arguments, which in the module body has to
//! be one that cannot throw — converting a BigInt to a number, or a symbol, does —
//! and the functions that throw on some arguments are listed apart, for a helper's
//! body only: `decodeURI("%")`, `encodeURI` of a lone surrogate,
//! `String.fromCodePoint(-1)`, `new Array(-1)`, and `WeakMap` or `WeakSet` given a
//! primitive key.
//!
//! `Symbol.for` is left out. It adds its key to the global symbol registry every
//! module shares.
//!
//! Every name is the global's only if the reference has no binding in the file,
//! which the caller checks. The environment's own globals are assumed to be as the
//! language defines them, which is what the source assumes too.

/// What a global function does to its arguments before it looks at them, which in
/// the module body decides whether it can throw.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Conversion {
    /// Nothing: any argument is read as it is.
    None,
    /// `ToString`, which runs an object's `toString` and throws on a symbol.
    ToString,
    /// `ToNumber`, which runs an object's `valueOf` and throws on a BigInt or a
    /// symbol.
    ToNumber,
    /// Shown as they are, the way the console shows a value. A format string's `%s`
    /// and the rest convert what follows it, which runs `toString` at most.
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
    /// The same, but some arguments make it throw: `new Array(-1)`.
    Throwing(Conversion),
    /// `Set`: iterates an array literal, if given one.
    Set,
    /// `Map`: iterates an array literal of array-literal entries, if given one.
    Map,
    /// `WeakSet` and `WeakMap`, read like `Set` and `Map`, except that an entry
    /// with a primitive key throws.
    WeakSet,
    WeakMap,
}

#[rustfmt::skip]
pub(crate) fn constructor(name: &str) -> Option<Constructor> {
    use Constructor::*;
    Some(match name {
        "Set" => Set,
        "Map" => Map,
        "WeakSet" => WeakSet,
        "WeakMap" => WeakMap,
        "Array" => Throwing(Conversion::None),
        "Object" | "Boolean" => Converting(Conversion::None),
        "String" | "Error" | "EvalError" | "RangeError" | "ReferenceError" | "SyntaxError"
        | "TypeError" | "URIError" => Converting(Conversion::ToString),
        "Date" | "Number" => Converting(Conversion::ToNumber),
        _ => return Option::None,
    })
}

/// A call with no side effects that throws on some arguments, which a helper's
/// body may make and the module body may not.
#[rustfmt::skip]
pub(crate) fn throwing(object: Option<&str>, name: &str) -> Option<(Conversion, Returns)> {
    use Conversion::*;
    use Returns::*;
    Some(match (object, name) {
        // A malformed escape, or a lone surrogate.
        (Option::None, "decodeURI" | "decodeURIComponent" | "encodeURI" | "encodeURIComponent") => {
            (ToString, Primitive)
        }
        (Option::None, "escape" | "unescape") => (ToString, Primitive),
        // A code point that is not an integer in range.
        (Some("String"), "fromCodePoint") => (ToNumber, Primitive),
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
