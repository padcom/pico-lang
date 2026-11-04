//! Semantic versions and version ranges (spec §8.3, decision 285), as Cargo reads them: `1.2.3`
//! and `^1.2.3` allow changes that keep the first non-zero part, `~1.2.3` patch changes,
//! comparisons (`>=1.2, <2`) and wildcards (`*`, `1.x`). A version with a pre-release part
//! (`1.0.0-beta.1`) matches only a range that names that same version's pre-release.

use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// `beta.1` of `1.0.0-beta.1` (empty for a release).
    pub pre: String,
}

impl Version {
    pub fn parse(s: &str) -> Result<Version, String> {
        let s = s.trim();
        // (Build metadata, `+…`, doesn't take part.)
        let s = s.split('+').next().unwrap_or("");
        let (nums, pre) = match s.split_once('-') {
            Some((n, p)) => (n, p.to_string()),
            None => (s, String::new()),
        };
        let parts: Vec<&str> = nums.split('.').collect();
        let bad =
            || format!("`{s}` isn't a version (expected `major.minor.patch`, such as `1.2.3`)");
        if parts.len() != 3 {
            return Err(bad());
        }
        let n = |p: &str| p.parse::<u64>().map_err(|_| bad());
        Ok(Version {
            major: n(parts[0])?,
            minor: n(parts[1])?,
            patch: n(parts[2])?,
            pre,
        })
    }

    /// What identifies a package's compatible versions (decision 285): the major, or for `0.x`
    /// the minor, or for `0.0.x` the patch.
    pub fn compat_key(&self) -> String {
        match (self.major, self.minor) {
            (0, 0) => format!("0.0.{}", self.patch),
            (0, m) => format!("0.{m}"),
            (m, _) => m.to_string(),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Version) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for Version {
    fn cmp(&self, o: &Version) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(o.major, o.minor, o.patch))
            // A pre-release comes before its release.
            .then_with(|| match (self.pre.is_empty(), o.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&o.pre),
            })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Exact,
    Greater,
    GreaterEq,
    Less,
    LessEq,
    Tilde,
    Caret,
    /// `*`, `1.x`, `1.2.*`: any version in the parts given.
    Wildcard,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Comparator {
    op: Op,
    major: u64,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: String,
}

/// A version range: comparators that must all hold (`>=1.2, <2`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range {
    text: String,
    all: Vec<Comparator>,
}

impl Range {
    pub fn parse(s: &str) -> Result<Range, String> {
        let text = s.trim().to_string();
        let mut all = Vec::new();
        if text == "*" || text.is_empty() {
            return Ok(Range { text, all });
        }
        for part in text.split(',') {
            all.push(comparator(part.trim()).map_err(|e| format!("`{text}`: {e}"))?);
        }
        Ok(Range { text, all })
    }

    pub fn matches(&self, v: &Version) -> bool {
        // A pre-release only for a comparator that names its version's pre-release.
        if !v.pre.is_empty()
            && !self.all.iter().any(|c| {
                !c.pre.is_empty()
                    && c.major == v.major
                    && c.minor == Some(v.minor)
                    && c.patch == Some(v.patch)
            })
        {
            return false;
        }
        self.all.iter().all(|c| c.matches(v))
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn comparator(s: &str) -> Result<Comparator, String> {
    let (op, rest) = [
        (">=", Op::GreaterEq),
        ("<=", Op::LessEq),
        (">", Op::Greater),
        ("<", Op::Less),
        ("=", Op::Exact),
        ("~", Op::Tilde),
        ("^", Op::Caret),
    ]
    .iter()
    .find_map(|(p, op)| s.strip_prefix(p).map(|r| (*op, r.trim())))
    .unwrap_or((Op::Caret, s));
    let (nums, pre) = match rest.split_once('-') {
        Some((n, p)) => (n, p.to_string()),
        None => (rest, String::new()),
    };
    let parts: Vec<&str> = nums.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return Err(format!("`{s}` isn't a version requirement"));
    }
    let mut wildcard = false;
    let mut nums = Vec::new();
    for p in &parts {
        if matches!(*p, "*" | "x" | "X") {
            wildcard = true;
            continue;
        }
        if wildcard {
            return Err(format!("`{s}`: nothing can follow a wildcard"));
        }
        nums.push(
            p.parse::<u64>()
                .map_err(|_| format!("`{s}` isn't a version requirement"))?,
        );
    }
    if nums.is_empty() {
        return Err(format!("`{s}` isn't a version requirement"));
    }
    let op = if wildcard { Op::Wildcard } else { op };
    Ok(Comparator {
        op,
        major: nums[0],
        minor: nums.get(1).copied(),
        patch: nums.get(2).copied(),
        pre,
    })
}

impl Comparator {
    fn lower(&self) -> Version {
        Version {
            major: self.major,
            minor: self.minor.unwrap_or(0),
            patch: self.patch.unwrap_or(0),
            pre: self.pre.clone(),
        }
    }

    fn matches(&self, v: &Version) -> bool {
        let lo = self.lower();
        let (maj, min) = (self.major, self.minor);
        match self.op {
            Op::Exact => {
                v.major == maj
                    && self.minor.is_none_or(|m| v.minor == m)
                    && self.patch.is_none_or(|p| v.patch == p)
                    && (self.patch.is_none() || v.pre == self.pre)
            }
            Op::Greater => match (self.minor, self.patch) {
                (Some(_), Some(_)) => *v > lo,
                (Some(m), None) => (v.major, v.minor) > (maj, m),
                _ => v.major > maj,
            },
            Op::GreaterEq => *v >= lo,
            Op::Less => *v < lo,
            Op::LessEq => match (self.minor, self.patch) {
                (Some(_), Some(_)) => *v <= lo,
                (Some(m), None) => (v.major, v.minor) <= (maj, m),
                _ => v.major <= maj,
            },
            Op::Tilde => {
                *v >= lo
                    && match min {
                        Some(m) => v.major == maj && v.minor == m,
                        None => v.major == maj,
                    }
            }
            Op::Caret => {
                *v >= lo
                    && match (maj, min, self.patch) {
                        (0, Some(0), Some(p)) => v.major == 0 && v.minor == 0 && v.patch == p,
                        (0, Some(m), _) => v.major == 0 && v.minor == m,
                        (m, _, _) => v.major == m,
                    }
            }
            Op::Wildcard => v.major == maj && self.minor.is_none_or(|m| v.minor == m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(range: &str, version: &str) -> bool {
        Range::parse(range)
            .unwrap()
            .matches(&Version::parse(version).unwrap())
    }

    #[test]
    fn ranges() {
        for (r, yes, no) in [
            ("^1.2.3", &["1.2.3", "1.9.0"][..], &["1.2.2", "2.0.0"][..]),
            ("1.2.3", &["1.2.3", "1.3.0"], &["2.0.0"]),
            ("^0.2.3", &["0.2.3", "0.2.9"], &["0.3.0", "0.2.2"]),
            ("^0.0.3", &["0.0.3"], &["0.0.4"]),
            ("^1.2", &["1.2.0", "1.5.1"], &["2.0.0", "1.1.9"]),
            ("~1.2.3", &["1.2.3", "1.2.9"], &["1.3.0"]),
            ("~1", &["1.0.0", "1.9.9"], &["2.0.0"]),
            (">=1.2, <2", &["1.2.0", "1.99.0"], &["2.0.0", "1.1.0"]),
            ("=1.2.3", &["1.2.3"], &["1.2.4"]),
            ("*", &["0.0.1", "7.0.0"], &[]),
            ("1.x", &["1.0.0", "1.7.2"], &["2.0.0"]),
            ("1.2.*", &["1.2.0", "1.2.8"], &["1.3.0"]),
            (
                "^1.0.0-beta.1",
                &["1.0.0-beta.1", "1.0.0", "1.4.0"],
                &["1.0.1-beta.1"],
            ),
        ] {
            for v in yes {
                assert!(ok(r, v), "{r} should match {v}");
            }
            for v in no {
                assert!(!ok(r, v), "{r} shouldn't match {v}");
            }
        }
        assert!(
            !ok("^1", "1.1.0-alpha"),
            "a pre-release only when asked for"
        );
    }

    #[test]
    fn versions_and_keys() {
        assert!(Version::parse("1.2").is_err());
        assert!(Range::parse("^1.x.3").is_err());
        assert_eq!(Version::parse("1.2.3").unwrap().compat_key(), "1");
        assert_eq!(Version::parse("0.4.1").unwrap().compat_key(), "0.4");
        assert_eq!(Version::parse("0.0.7").unwrap().compat_key(), "0.0.7");
        assert!(Version::parse("1.0.0-rc.1").unwrap() < Version::parse("1.0.0").unwrap());
    }
}
