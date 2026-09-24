//! Globals whose calls only compute a value.
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
//! throw on a primitive key.
//!
//! Every name is the global's only if the reference has no binding in the file,
//! which the caller checks. The environment's own globals are assumed to be as the
//! language defines them, which is what the source assumes too.
//!
//! Portions of this file are derived from oxc, used under the MIT licence:
//!
//! MIT License
//!
//! Copyright (c) 2024-present VoidZero Inc. & Contributors
//! Copyright (c) 2023 Boshen
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

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

/// A call to a global function, `name(...)`, that only computes a value.
#[rustfmt::skip]
pub(crate) fn function(name: &str) -> Option<(Conversion, Returns)> {
    use Conversion::*;
    use Returns::*;
    Some(match name {
        "isFinite" | "isNaN" => (ToNumber, Primitive),
        "parseFloat" | "parseInt" => (ToString, Primitive),
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
/// that only computes a value.
#[rustfmt::skip]
pub(crate) fn method(object: &str, method: &str) -> Option<(Conversion, Returns)> {
    use Conversion::*;
    use Returns::*;
    Some(match (object, method) {
        ("Math", method) if is_math_method(method) => (ToNumber, Primitive),
        ("Number", "isFinite" | "isInteger" | "isNaN" | "isSafeInteger") => (None, Primitive),
        ("Number", "parseFloat" | "parseInt") => (ToString, Primitive),
        ("Array", "isArray") => (None, Primitive),
        ("Array", "of") => (None, Other),
        ("ArrayBuffer", "isView") => (None, Primitive),
        ("Object", "is") => (None, Primitive),
        ("Date", "now") => (None, Primitive),
        ("Date", "parse") => (ToString, Primitive),
        ("Date", "UTC") => (ToNumber, Primitive),
        ("String", "fromCharCode") => (ToNumber, Primitive),
        // A symbol, which no conversion may be applied to.
        ("Symbol", "for") => (ToString, Other),
        _ => return Option::None,
    })
}

/// A constructor, `new name(...)`, that only builds a value.
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
fn is_math_method(method: &str) -> bool {
    matches!(method,
        "abs" | "acos" | "acosh" | "asin" | "asinh" | "atan" | "atan2" | "atanh"
        | "cbrt" | "ceil" | "clz32" | "cos" | "cosh" | "exp" | "expm1" | "floor"
        | "fround" | "hypot" | "imul" | "log" | "log10" | "log1p" | "log2" | "max"
        | "min" | "pow" | "random" | "round" | "sign" | "sin" | "sinh" | "sqrt"
        | "tan" | "tanh" | "trunc"
    )
}
