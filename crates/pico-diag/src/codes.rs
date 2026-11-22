//! Error codes. Ranges: `E00xx` lexical, `E01xx` syntax, `E02xx` names and modules,
//! `E03xx` types. Lint warnings are `W0xxx` ([`LINTS`], spec §14.7). Codes are never reused
//! once assigned.

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Code(pub u16);

/// The bit that marks a lint warning's code (`W0001` is `Code(WARNING | 1)`).
pub const WARNING: u16 = 0x8000;

impl Code {
    /// A lint warning's code, `W{n:04}`.
    pub const fn warning(n: u16) -> Code {
        Code(WARNING | n)
    }

    pub fn is_warning(self) -> bool {
        self.0 & WARNING != 0
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_warning() {
            write!(f, "W{:04}", self.0 & !WARNING)
        } else {
            write!(f, "E{:04}", self.0)
        }
    }
}

macro_rules! lints {
    ($($name:ident = $num:literal: $rule:literal: $desc:literal,)*) => {
        $(#[doc = $desc] pub const $name: Code = Code::warning($num);)*

        /// Every lint warning: its code, its rule in `project.json`'s `"lint"`, and a one-line
        /// description.
        pub const LINTS: &[(Code, &str, &str)] = &[$(($name, $rule, $desc),)*];
    };
}

lints! {
    LINT_NAMING = 1: "naming": "a name doesn't follow the naming conventions",
    LINT_UNUSED = 2: "unused": "a variable, parameter, private member or import is never used",
    LINT_COMPLEXITY = 3: "complexity": "a function's cyclomatic complexity is above the maximum",
    LINT_BLANK_LINE = 4: "blankLines": "a blank line is missing before a statement or declaration",
    LINT_DOCS = 5: "docs": "an exported declaration has no doc comment",
    LINT_EXPLICIT_RETURN = 6: "explicitReturn": "a function's result is a simple value without `return`",
    LINT_MAX_PARAMS = 7: "maxParams": "a function has more parameters than the maximum",
    LINT_MAX_DEPTH = 8: "maxDepth": "blocks are nested deeper than the maximum",
    LINT_MAX_LINES = 9: "maxLines": "a function has more lines than the maximum",
    LINT_MAX_NESTED_CALLBACKS = 10: "maxNestedCallbacks": "callbacks are nested deeper than the maximum",
    LINT_MAX_STATEMENTS_PER_LINE = 11: "maxStatementsPerLine": "a line has more statements than the maximum",
    LINT_MAX_LEN = 12: "maxLen": "a line is longer than the maximum",
    LINT_SHADOW = 13: "shadow": "a declaration hides a variable of an enclosing scope",
    LINT_UNUSED_EXPRESSIONS = 14: "unusedExpressions": "an expression's value is dropped and it has no effect",
    LINT_UNREACHABLE = 15: "unreachable": "code after `return`, `throw`, `break` or `continue` never runs",
    LINT_SELF_COMPARE = 16: "selfCompare": "a value is compared with itself",
    LINT_SELF_ASSIGN = 17: "selfAssign": "a variable is assigned to itself",
    LINT_CONSTANT_BINARY = 18: "constantBinary": "an operation's result doesn't depend on its operands",
    LINT_UNSAFE_FINALLY = 19: "unsafeFinally": "`finally` exits with `return`, `throw`, `break` or `continue`",
    LINT_REQUIRE_YIELD = 20: "requireYield": "a generator has no `yield`",
    LINT_FOR_DIRECTION = 21: "forDirection": "a `for` loop's update moves its counter away from the condition",
    LINT_EMPTY_BLOCK = 22: "emptyBlock": "a block is empty",
    LINT_DUPE_ELSE_IF = 23: "dupeElseIf": "an `else if` repeats an earlier condition",
    LINT_USELESS_CATCH = 24: "uselessCatch": "a `catch` only rethrows what it caught",
    LINT_TEMPLATE_IN_STRING = 25: "templateInString": "a plain string contains a `${…}` substitution",
    LINT_LOOP_FUNC = 26: "loopFunc": "a closure in a loop captures a variable the loop changes",
    LINT_FLOATING_PROMISE = 27: "floatingPromise": "a promise is dropped without `await` or `catch`",
    LINT_AWAIT_IN_LOOP = 28: "awaitInLoop": "`await` inside a loop",
    LINT_REQUIRE_AWAIT = 29: "requireAwait": "an `async` function has no `await`",
    LINT_PREFER_AWAIT = 31: "preferAwait": "`then`/`catch`/`finally` or a callback where `await` would do",
    LINT_PROMISE_RETURN_WRAP = 32: "promiseReturnWrap": "a promise callback wraps its result in `Promise.resolve`/`reject`",
    LINT_PROMISE_PARAM_NAMES = 33: "promiseParamNames": "a promise executor's parameters aren't `resolve` and `reject`",
    LINT_PREFER_CONST = 34: "preferConst": "a `let` is never reassigned",
    LINT_OBJECT_SHORTHAND = 35: "objectShorthand": "an object literal field repeats its name (`x: x`)",
    LINT_USELESS_RENAME = 36: "uselessRename": "a destructuring, import or export renames a name to itself",
    LINT_OPERATOR_ASSIGNMENT = 37: "operatorAssignment": "`x = x op y` where `x op= y` would do",
    LINT_PREFER_TEMPLATE = 38: "preferTemplate": "a string is built with `+` where a template would do",
    LINT_USELESS_CONCAT = 39: "uselessConcat": "two string literals are joined with `+`",
    LINT_ARROW_BODY_STYLE = 40: "arrowBodyStyle": "an arrow function's block body holds a single expression",
    LINT_PREFER_ARROW_CALLBACK = 41: "preferArrowCallback": "a `function` expression is passed as a callback",
    LINT_FUNC_STYLE = 42: "funcStyle": "a `function` expression is assigned to a variable",
    LINT_LONELY_IF = 43: "lonelyIf": "an `else` block holds only an `if`",
    LINT_LONE_BLOCKS = 44: "loneBlocks": "a block statement scopes nothing",
    LINT_USELESS_RETURN = 45: "uselessReturn": "a `return` at the end of a function does nothing",
    LINT_USELESS_CONSTRUCTOR = 46: "uselessConstructor": "a constructor does nothing the default one wouldn't",
    LINT_PREFER_DESTRUCTURING = 47: "preferDestructuring": "a variable is initialized from a field of the same name",
    LINT_PREFER_EXPONENTIATION = 48: "preferExponentiation": "`Math.pow` where `**` would do",
    LINT_YODA = 49: "yoda": "a comparison puts the literal first",
    LINT_GROUPED_ACCESSORS = 50: "groupedAccessors": "a getter and its setter aren't next to each other, getter first",
    LINT_CURLY = 51: "curly": "a body without braces doesn't end on its statement's line",
    LINT_EXTRA_PARENS = 52: "extraParens": "parentheses that change nothing",
    LINT_MIXED_OPERATORS = 53: "mixedOperators": "operators of different precedence are mixed without parentheses",
    LINT_COMMA_DANGLE = 54: "commaDangle": "a trailing comma is missing in a multi-line list or present in a one-line list",
    LINT_BRACE_STYLE = 55: "braceStyle": "braces aren't placed in the one true brace style",
    LINT_OPERATOR_LINEBREAK = 56: "operatorLinebreak": "a line breaks after an operator instead of before it",
    LINT_MULTILINE_TERNARY = 57: "multilineTernary": "a multi-line conditional expression doesn't put each part on its own line",
    LINT_SPACED_COMMENT = 58: "spacedComment": "a comment doesn't start with a space",
    LINT_EXTRA_SEMI = 59: "extraSemi": "a semicolon that changes nothing",
    LINT_QUOTES = 60: "quotes": "a string uses the other kind of quotes",
    LINT_DOC_TAGS = 61: "docTags": "a doc comment's tags don't match the declaration",
    LINT_DOC_FORMAT = 62: "docFormat": "a doc comment's layout is off",
    LINT_UNDERSCORE_DANGLE = 63: "underscoreDangle": "a used name starts or ends with `_`",
    LINT_PREFER_NAMED_CAPTURE_GROUP = 64: "preferNamedCaptureGroup": "a regex has an unnamed capture group",
}

impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

macro_rules! codes {
    ($($name:ident = $num:literal: $desc:literal,)*) => {
        $(#[doc = $desc] pub const $name: Code = Code($num);)*

        /// Every code with a one-line description (for `pico explain`).
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
    DUPLICATE_FIELD = 312: "field or map key given twice in an object literal",
    UNKNOWN_OBJECT_TYPE = 313: "object literal without a known struct or map type",
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
    EXPORT_TYPE = 355: "an `@export`ed or `extern` function whose signature can't cross to JavaScript or C",
    UNAVAILABLE = 356: "a module or feature that isn't available on the target",
    REACTIVE = 357: "reactive state used where it isn't allowed",
    GENERIC_VIRTUAL = 358: "a generic method that's virtual, abstract, an override or an interface method",
    // Internal
    NOT_IMPLEMENTED = 900: "feature not implemented yet",
}

include!(concat!(env!("OUT_DIR"), "/explanations.rs"));

/// The long-form explanation of a code (`pico explain E0308`, spec §14.2): Markdown from
/// `docs/errors/<code>.md`.
pub fn explain(code: Code) -> Option<&'static str> {
    let key = code.to_string();
    EXPLANATIONS
        .iter()
        .find(|(c, _)| *c == key)
        .map(|(_, t)| *t)
}

/// A code from its written form: `E0308`, `e0308`, `308`, `W0001`.
pub fn parse(s: &str) -> Option<Code> {
    let s = s.trim();
    let (warning, digits) = match s.chars().next()? {
        'E' | 'e' => (false, &s[1..]),
        'W' | 'w' => (true, &s[1..]),
        _ => (false, s),
    };
    let n: u16 = digits.parse().ok()?;
    let code = if warning { Code::warning(n) } else { Code(n) };
    describe(code).map(|_| code)
}

/// The code of lint rule `rule` (its name in `project.json`'s `"lint"`).
pub fn lint_code(rule: &str) -> Option<Code> {
    LINTS.iter().find(|(_, r, _)| *r == rule).map(|(c, ..)| *c)
}

pub fn describe(code: Code) -> Option<&'static str> {
    ALL.iter()
        .find(|(c, _)| *c == code)
        .map(|(_, d)| *d)
        .or_else(|| LINTS.iter().find(|(c, ..)| *c == code).map(|(.., d)| *d))
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
        for (c, ..) in LINTS {
            assert!(seen.insert(c.0), "duplicate code {c}");
        }
        assert_eq!(MISMATCHED_TYPES.to_string(), "E0308");
        assert_eq!(LINT_DOCS.to_string(), "W0005");
        assert!(LINT_DOCS.is_warning() && !MISMATCHED_TYPES.is_warning());
        assert_eq!(describe(LINT_UNUSED), Some(LINTS[1].2));
        assert_eq!(
            describe(UNTERMINATED_STRING),
            Some("unterminated string literal")
        );
    }
}
