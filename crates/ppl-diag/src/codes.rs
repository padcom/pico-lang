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
