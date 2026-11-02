//! Compile-time checking of regex literals (spec §1, §13.7): the same rules and messages as
//! the runtime parser in `stdlib/core/regex.ppl` (`__RParser`), so `/(abc/` is a compile
//! error and a literal that passes can't fail at runtime.

/// Checks `pattern` and `flags`; the message JavaScript would give otherwise.
pub fn validate(pattern: &str, flags: &str) -> Result<(), String> {
    let mut seen = String::new();
    for f in flags.chars() {
        if !"dgimsuvy".contains(f) || seen.contains(f) {
            return Err(format!(
                "Invalid flags supplied to RegExp constructor '{flags}'"
            ));
        }
        seen.push(f);
    }
    let src = if pattern.is_empty() { "(?:)" } else { pattern };
    let mut p = Parser {
        p: src.chars().map(|c| c as i64).collect(),
        i: 0,
        groups: 0,
        names: Vec::new(),
        refs: Vec::new(),
        max_backref: 0,
    };
    p.parse()
        .map_err(|m| format!("Invalid regular expression: /{src}/{flags}: {m}"))
}

struct Parser {
    p: Vec<i64>,
    i: usize,
    groups: i64,
    names: Vec<String>,
    refs: Vec<String>,
    max_backref: i64,
}

type R<T> = Result<T, &'static str>;

/// What an atom was, for "Nothing to repeat".
#[derive(PartialEq)]
enum Atom {
    Repeatable,
    Assertion,
}

fn hex_value(c: i64) -> Option<i64> {
    char::from_u32(c as u32)?.to_digit(16).map(i64::from)
}

impl Parser {
    fn peek(&self) -> i64 {
        self.p.get(self.i).copied().unwrap_or(-1)
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == c as i64 {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn parse(&mut self) -> R<()> {
        self.alternation()?;
        if self.i < self.p.len() {
            return Err("Unmatched ')'");
        }
        for r in &self.refs {
            if !self.names.contains(r) {
                return Err("Invalid named capture referenced");
            }
        }
        if self.max_backref > self.groups {
            return Err("Invalid escape");
        }
        Ok(())
    }

    fn alternation(&mut self) -> R<()> {
        self.sequence()?;
        while self.eat('|') {
            self.sequence()?;
        }
        Ok(())
    }

    fn sequence(&mut self) -> R<()> {
        while self.i < self.p.len() && self.peek() != '|' as i64 && self.peek() != ')' as i64 {
            self.quantified()?;
        }
        Ok(())
    }

    fn number(&mut self) -> Option<i64> {
        let mut n: i64 = 0;
        let mut digits = 0;
        while (48..=57).contains(&self.peek()) {
            n = (n * 10 + self.peek() - 48).min(100_000_000);
            self.i += 1;
            digits += 1;
        }
        (digits > 0).then_some(n)
    }

    /// `{n}`, `{n,}`, `{n,m}`: (min, max or -1); `None` (position unchanged) if it isn't one.
    fn braces(&mut self) -> Option<(i64, i64)> {
        let save = self.i;
        if !self.eat('{') {
            return None;
        }
        let Some(min) = self.number() else {
            self.i = save;
            return None;
        };
        let max = if self.eat(',') {
            self.number().unwrap_or(-1)
        } else {
            min
        };
        if !self.eat('}') {
            self.i = save;
            return None;
        }
        Some((min, max))
    }

    fn quantified(&mut self) -> R<()> {
        let atom = self.atom()?;
        let c = self.peek();
        let quantified = if c == '*' as i64 || c == '+' as i64 || c == '?' as i64 {
            self.i += 1;
            true
        } else if c == '{' as i64 {
            match self.braces() {
                Some((min, max)) => {
                    if max >= 0 && max < min {
                        return Err("numbers out of order in {} quantifier");
                    }
                    true
                }
                None => false,
            }
        } else {
            false
        };
        if quantified {
            if atom == Atom::Assertion {
                return Err("Nothing to repeat");
            }
            self.eat('?');
        }
        Ok(())
    }

    fn atom(&mut self) -> R<Atom> {
        let c = self.peek();
        self.i += 1;
        match char::from_u32(c as u32).unwrap_or('\0') {
            '^' | '$' => Ok(Atom::Assertion),
            '*' | '+' | '?' => Err("Nothing to repeat"),
            '{' => {
                self.i -= 1;
                if self.braces().is_some() {
                    return Err("Nothing to repeat");
                }
                self.i += 1;
                Ok(Atom::Repeatable)
            }
            '(' => self.group(),
            '[' => {
                self.char_class()?;
                Ok(Atom::Repeatable)
            }
            '\\' => self.escape(),
            _ => Ok(Atom::Repeatable),
        }
    }

    fn close(&mut self) -> R<()> {
        if self.eat(')') {
            Ok(())
        } else {
            Err("Unterminated group")
        }
    }

    fn group(&mut self) -> R<Atom> {
        if self.eat('?') {
            let c = self.peek();
            self.i += 1;
            match char::from_u32(c as u32).unwrap_or('\0') {
                ':' => {
                    self.alternation()?;
                    self.close()?;
                    return Ok(Atom::Repeatable);
                }
                '=' | '!' => {
                    self.alternation()?;
                    self.close()?;
                    return Ok(Atom::Repeatable);
                }
                '<' => {
                    if self.eat('=') || self.eat('!') {
                        self.alternation()?;
                        self.close()?;
                        return Ok(Atom::Assertion);
                    }
                    let name = self.group_name()?;
                    if self.names.contains(&name) {
                        return Err("Duplicate capture group name");
                    }
                    self.names.push(name);
                    self.groups += 1;
                    self.alternation()?;
                    self.close()?;
                    return Ok(Atom::Repeatable);
                }
                _ => return Err("Invalid group"),
            }
        }
        self.groups += 1;
        self.alternation()?;
        self.close()?;
        Ok(Atom::Repeatable)
    }

    fn group_name(&mut self) -> R<String> {
        let mut name = String::new();
        loop {
            let c = self.peek();
            if c < 0 {
                return Err("Invalid capture group name");
            }
            self.i += 1;
            if c == '>' as i64 {
                break;
            }
            let ch = char::from_u32(c as u32).unwrap_or('\0');
            let ok = ch.is_ascii_alphabetic()
                || ch == '_'
                || ch == '$'
                || (ch.is_ascii_digit() && !name.is_empty())
                || c > 127;
            if !ok {
                return Err("Invalid capture group name");
            }
            name.push(ch);
        }
        if name.is_empty() {
            return Err("Invalid capture group name");
        }
        Ok(name)
    }

    fn hex(&mut self, count: usize) -> R<i64> {
        let mut v = 0;
        for _ in 0..count {
            let d = hex_value(self.peek()).ok_or("Invalid Unicode escape")?;
            v = v * 16 + d;
            self.i += 1;
        }
        Ok(v)
    }

    /// A character escape's code point, or `None` for a class escape (`\d`, …).
    fn escape_char(&mut self, in_class: bool) -> R<Option<i64>> {
        let c = self.peek();
        if c < 0 {
            return Err("\\ at end of pattern");
        }
        self.i += 1;
        let ch = char::from_u32(c as u32).unwrap_or('\0');
        Ok(Some(match ch {
            'd' | 'D' | 'w' | 'W' | 's' | 'S' => return Ok(None),
            'n' => 10,
            'r' => 13,
            't' => 9,
            'v' => 11,
            'f' => 12,
            'b' if in_class => 8,
            '0' if !(48..=57).contains(&self.peek()) => 0,
            'x' => self.hex(2)?,
            'u' => {
                if self.eat('{') {
                    let mut v = 0;
                    let mut digits = 0;
                    while self.peek() != '}' as i64 {
                        let d = hex_value(self.peek()).ok_or("Invalid Unicode escape")?;
                        v = v * 16 + d;
                        digits += 1;
                        self.i += 1;
                        if v > 0x10FFFF {
                            return Err("Invalid Unicode escape");
                        }
                    }
                    self.i += 1;
                    if digits == 0 {
                        return Err("Invalid Unicode escape");
                    }
                    v
                } else {
                    self.hex(4)?
                }
            }
            'c' => {
                let l = self.peek();
                if char::from_u32(l as u32).is_some_and(|x| x.is_ascii_alphabetic()) {
                    self.i += 1;
                    l % 32
                } else {
                    return Err("Invalid unicode escape");
                }
            }
            '^' | '$' | '\\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
            | '/' => c,
            '-' if in_class => c,
            _ => return Err("Invalid escape"),
        }))
    }

    fn escape(&mut self) -> R<Atom> {
        let c = self.peek();
        if c == 'b' as i64 || c == 'B' as i64 {
            self.i += 1;
            return Ok(Atom::Assertion);
        }
        if (49..=57).contains(&c) {
            let n = self.number().unwrap_or(0);
            self.max_backref = self.max_backref.max(n);
            return Ok(Atom::Repeatable);
        }
        if c == 'k' as i64 {
            self.i += 1;
            if !self.eat('<') {
                return Err("Invalid named reference");
            }
            let name = self.group_name()?;
            self.refs.push(name);
            return Ok(Atom::Repeatable);
        }
        self.escape_char(false)?;
        Ok(Atom::Repeatable)
    }

    fn char_class(&mut self) -> R<()> {
        self.eat('^');
        loop {
            let c = self.peek();
            if c < 0 {
                return Err("Unterminated character class");
            }
            if c == ']' as i64 {
                self.i += 1;
                return Ok(());
            }
            let lo = self.class_atom()?;
            if self.peek() == '-' as i64
                && self.i + 1 < self.p.len()
                && self.p[self.i + 1] != ']' as i64
            {
                self.i += 1;
                let hi = self.class_atom()?;
                match (lo, hi) {
                    (Some(lo), Some(hi)) if hi < lo => {
                        return Err("Range out of order in character class");
                    }
                    (Some(_), Some(_)) => {}
                    _ => return Err("Invalid character class"),
                }
            }
        }
    }

    fn class_atom(&mut self) -> R<Option<i64>> {
        let c = self.peek();
        self.i += 1;
        if c == '\\' as i64 {
            self.escape_char(true)
        } else {
            Ok(Some(c))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn valid_patterns() {
        for p in [
            r"\d+(\.\d+)?",
            r"^(?<year>\d{4})-(?<m>\d\d)$",
            r"(a|b)*c{2,}?",
            r"[a-z\d_-]+",
            r"(?<=\$)\d+(?!px)",
            r"(\w)\1",
            r"\k<x>(?<x>a)",
            r"a{,5}",
            r"",
            r"\u{1F600}\x41\cJ",
        ] {
            assert_eq!(validate(p, "gimsuy"), Ok(()), "{p}");
        }
    }

    #[test]
    fn invalid_patterns() {
        for (p, msg) in [
            ("(abc", "Unterminated group"),
            ("abc)", "Unmatched ')'"),
            ("*a", "Nothing to repeat"),
            ("a**", "Nothing to repeat"),
            ("^*", "Nothing to repeat"),
            ("[b-a]", "Range out of order in character class"),
            ("[abc", "Unterminated character class"),
            ("a{3,2}", "numbers out of order in {} quantifier"),
            ("(?<n>a)(?<n>b)", "Duplicate capture group name"),
            (r"\k<nope>", "Invalid named capture referenced"),
            (r"(a)\2", "Invalid escape"),
            (r"\q", "Invalid escape"),
            ("(?x)", "Invalid group"),
            ("a\\", "\\ at end of pattern"),
        ] {
            let e = validate(p, "").unwrap_err();
            assert!(e.ends_with(msg), "{p}: {e}");
        }
        assert!(validate("a", "gg").is_err());
        assert!(validate("a", "x").is_err());
    }
}
