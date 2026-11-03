//! Error codes. Ranges: `E00xx` lexical, `E01xx` syntax, `E02xx` names and modules,
//! `E03xx` types. Codes are never reused once assigned.

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Code(pub u16);

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "E{:04}", self.0)
    }
}

impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

macro_rules! codes {
    ($($name:ident = $num:literal: $desc:literal,)*) => {
        $(#[doc = $desc] pub const $name: Code = Code($num);)*

        /// Every code with a one-line description (for `ppl explain`).
        pub const ALL: &[(Code, &str)] = &[$(($name, $desc),)*];
    };
}

codes! {
    // Lexical
    UNTERMINATED_STRING = 1: "unterminated string literal",
    UNTERMINATED_COMMENT = 2: "unterminated block comment",
    INVALID_ESCAPE = 3: "invalid escape sequence",
    INVALID_NUMBER = 4: "invalid numeric literal or suffix",
    NUMBER_TOO_LARGE = 5: "integer literal too large",
    UNEXPECTED_CHAR = 6: "unexpected character",
    UNTERMINATED_TEMPLATE = 7: "unterminated template literal",
    UNTERMINATED_REGEX = 8: "unterminated regex literal",
    INVALID_MULTILINE = 9: "malformed multiline string literal",
    INVALID_REGEX = 10: "invalid regular expression literal",
    // Syntax
    UNEXPECTED_TOKEN = 100: "unexpected token",
    UNCLOSED_DELIMITER = 101: "unclosed delimiter",
    INVALID_ASSIGN_TARGET = 102: "invalid assignment target",
    POSITIONAL_AFTER_NAMED = 103: "positional argument after a named argument",
    MISSING_SEPARATOR = 104: "expected a newline or `;` between statements",
    INVALID_MODIFIER = 105: "modifier not allowed here",
    MIXED_NULLISH = 106: "`??` mixed with `&&` or `||` without parentheses",
    // Names
    UNKNOWN_NAME = 201: "cannot find a name in this scope",
    USE_BEFORE_DECL = 202: "name used before its declaration",
    DUPLICATE_DECL = 203: "name declared twice in the same scope",
    ASSIGN_TO_CONST = 204: "assignment to a `const` binding",
    BREAK_OUTSIDE_LOOP = 205: "`break` or `continue` outside a loop",
    UNKNOWN_TYPE = 206: "cannot find a type",
    MISSING_PARAM_TYPE = 207: "parameter without a type annotation",
    NOT_A_VALUE = 208: "a type or built-in used as a value",
    BREAK_VALUE_IN_WHILE = 209: "`break` with a value outside `loop`",
    RECURSIVE_STRUCT = 210: "struct that contains itself by value",
    // Types
    LITERAL_OUT_OF_RANGE = 301: "literal out of range for its type",
    WRONG_ARG_COUNT = 302: "wrong number of arguments",
    NOT_CALLABLE = 303: "called value is not a function",
    INVALID_OPERANDS = 304: "operator can't be applied to these types",
    INVALID_CAST = 305: "invalid `as` conversion",
    RECURSIVE_NEEDS_RETURN_TYPE = 306: "recursive function without a return type",
    MISMATCHED_TYPES = 308: "mismatched types",
    UNKNOWN_MEMBER = 309: "no such member",
    INVALID_FORMAT_SPEC = 310: "invalid format spec",
    MISSING_FIELDS = 311: "struct literal without all fields",
    DUPLICATE_FIELD = 312: "field given twice in a struct literal",
    UNKNOWN_OBJECT_TYPE = 313: "object literal without a known struct type",
    TUPLE_INDEX = 314: "tuple index that isn't a constant in range",
    UNKNOWN_ELEMENT_TYPE = 315: "array literal without a known element type",
    WRONG_LENGTH = 316: "array or tuple literal with the wrong number of elements",
    NON_EXHAUSTIVE = 317: "non-exhaustive `match`",
    PATTERN_MISMATCH = 318: "pattern that can't match the matched value's type",
    PRIVATE_MEMBER = 319: "use of a private member outside its class",
    UNASSIGNED_FIELD = 320: "field not definitely assigned by a constructor",
    MISSING_OVERRIDE = 321: "method overrides a virtual method without `override`",
    BAD_OVERRIDE = 322: "override of a method that can't be overridden",
    ABSTRACT_NOT_IMPLEMENTED = 323: "concrete class that leaves an abstract method unimplemented",
    ABSTRACT_NEW = 324: "`new` of an abstract class",
    OVERRIDE_SIGNATURE = 325: "override with a different signature",
    BAD_CTOR_CALL = 326: "`super(…)`/`this(…)` misplaced, missing or without a matching constructor",
    MISSING_IMPL = 327: "type that doesn't implement an interface method",
    CONFLICTING_DEFAULTS = 328: "conflicting default methods from two interfaces",
    CANT_INFER = 329: "type argument that can't be inferred",
    UNSATISFIED_BOUND = 330: "type argument that doesn't meet its bound",
    BAD_TAILCALL = 331: "`tailcall` that can't be a guaranteed tail call",
    AMBIGUOUS_OVERLOAD = 332: "call that fits several overloads equally well",
    NO_OVERLOAD = 333: "call that fits no overload",
    READONLY_FIELD = 334: "assignment to a `readonly` field outside a constructor",
    IMMUTABLE_MUTATION = 335: "mutation through an `immutable` view or in a `readonly` method",
    UNHANDLED_ERROR = 336: "a checked error is neither caught nor declared in `throws`",
    NOT_THROWABLE = 337: "`throw` of a value that isn't an `Error`",
    LIBRARY_LET = 338: "module-level `let` in a library module",
    INIT_CYCLE = 339: "a cycle in module initialization",
    NOT_EXPORTED = 340: "import of a name the module doesn't export",
    UNKNOWN_MODULE = 341: "import of a module that can't be found",
    UNSAFE_NEEDED = 342: "an unsafe operation outside `unsafe` code",
    POD_REFERENCE = 343: "a `@pod` struct with a field that isn't plain data",
    ARENA_ESCAPE = 344: "a reference allocated in an arena outlives it",
    AWAIT_OUTSIDE_ASYNC = 345: "`await` outside an async function",
    WORKER_CAPTURES = 346: "an inline worker's function captures variables",
    CONST_PANIC = 347: "a constant expression that always panics (overflow, division by zero, …)",
    COMPTIME_FAILED = 348: "comptime evaluation panicked or threw",
    COMPTIME_LIMIT = 349: "comptime evaluation exceeded its step limit",
    COMPTIME_VALUE = 350: "a comptime value that can't be part of the program (an object or a function)",
    COMPTIME_ONLY = 351: "a comptime-only operation outside comptime code",
    COMPTIME_CAPTURE = 352: "comptime code that uses a runtime variable",
    COMPTIME_ARG = 353: "a `comptime` argument that isn't a constant",
    DECORATOR = 354: "an error reported by a decorator, or a decorator used wrongly",
    EXPORT_TYPE = 355: "an `@export`ed function whose signature can't cross to JavaScript or C",
    // Internal
    NOT_IMPLEMENTED = 900: "feature not implemented yet",
}

pub fn describe(code: Code) -> Option<&'static str> {
    ALL.iter().find(|(c, _)| *c == code).map(|(_, d)| *d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_formatted() {
        let mut seen = std::collections::HashSet::new();
        for (c, _) in ALL {
            assert!(seen.insert(c.0), "duplicate code {c}");
        }
        assert_eq!(MISMATCHED_TYPES.to_string(), "E0308");
        assert_eq!(
            describe(UNTERMINATED_STRING),
            Some("unterminated string literal")
        );
    }
}
