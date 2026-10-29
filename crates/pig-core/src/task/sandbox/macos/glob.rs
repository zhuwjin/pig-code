//! Seatbelt glob translation.
//!
//! POSIX ERE has no inline flags and silently drops invalid rules, so this
//! module parses the glob subset `globset` accepts and renders it explicitly.
//! Anything it cannot represent is returned as an error; the caller aborts
//! the launch rather than dropping a deny rule.

/// Translate one normalized glob into a Seatbelt regex.
///
/// The semantics match the Windows matcher: case-insensitive, `*` and `?` never
/// cross a directory separator, a `**/` component matches any depth (including
/// none), `**` elsewhere matches without crossing a separator unless it follows
/// a separator, and `{a,b}`/`[a-z]`/`[!a-z]` are supported. Anchored at both
/// ends.
///
/// **No non-capturing groups.** Seatbelt's regex dialect is POSIX ERE, where
/// `(?:…)` is not valid: `sandbox-exec` silently drops a rule whose regex fails
/// to compile, which looks exactly like "the deny is not enforced". Verified
/// against the kernel — `^(?:.*/)?\.env$` let `.env` through, `^(.*/)?\.env$`
/// refuses it — so `**/` translates to a *capturing* group.
///
/// **Fail closed.** A pattern this translator cannot represent exactly returns
/// an error, and the launch is refused. Dropping the rule instead (the old
/// behaviour for anything it did not understand) silently narrowed the policy;
/// the parser below therefore rejects unclosed classes/alternates, empty
/// branches, control characters and bracket members POSIX ERE cannot express
/// unambiguously.
pub(super) fn seatbelt_regex_for_glob(pattern: &str) -> Result<String, String> {
    let normalized = pattern.replace('\\', "/");
    if normalized.is_empty() {
        return Err("the deny rule is empty".to_string());
    }
    if normalized.chars().any(char::is_control) {
        return Err(format!(
            "the deny rule `{pattern}` contains a control character that cannot be represented"
        ));
    }
    if normalized.contains("\"#") {
        return Err(format!(
            "the deny rule `{pattern}` contains `\"#`, which cannot be embedded in a seatbelt profile"
        ));
    }
    let mut parser = GlobParser {
        chars: normalized.chars().peekable(),
    };
    let body = parser.parse()?;
    Ok(format!("^{body}$"))
}

/// Parser for the glob subset the matcher (globset) accepts, rendered straight
/// into POSIX ERE. Escapes never reach this parser: `normalize_pattern` turns
/// backslashes into separators before either backend sees the rule.
struct GlobParser<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl GlobParser<'_> {
    fn parse(&mut self) -> Result<String, String> {
        let branches = self.parse_alternates(false)?;
        if branches.len() == 1 {
            Ok(branches.into_iter().next().unwrap_or_default())
        } else {
            Ok(format!("({})", branches.join("|")))
        }
    }

    /// Parse until the closing `}` (inside braces) or end of input. Returns the
    /// rendered alternatives; an empty alternative is rejected because globset's
    /// parser rejects it too and POSIX ERE has no reliable empty branch.
    fn parse_alternates(&mut self, in_brace: bool) -> Result<Vec<String>, String> {
        let mut branches: Vec<String> = Vec::new();
        let mut current = String::new();
        while let Some(ch) = self.chars.next() {
            match ch {
                '}' if in_brace => {
                    if current.is_empty() {
                        return Err("the deny rule has an empty {} alternative".to_string());
                    }
                    branches.push(current);
                    return Ok(branches);
                }
                '}' => return Err("the deny rule has an unmatched `}`".to_string()),
                ',' if in_brace => {
                    if current.is_empty() {
                        return Err("the deny rule has an empty {} alternative".to_string());
                    }
                    branches.push(std::mem::take(&mut current));
                }
                '{' => {
                    let alternatives = self.parse_alternates(true)?;
                    current.push('(');
                    current.push_str(&alternatives.join("|"));
                    current.push(')');
                }
                '*' => {
                    if self.chars.peek() == Some(&'*') {
                        self.chars.next();
                        let at_start = current.is_empty();
                        let after_separator = current.ends_with('/');
                        let at_branch_end = matches!(self.chars.peek(), None | Some('}' | ','));
                        if self.chars.peek() == Some(&'/') && (at_start || after_separator) {
                            self.chars.next();
                            // `**/` matches any depth, including none. The
                            // preceding separator (if any) was already emitted.
                            current.push_str("(.*/)?");
                        } else if at_start && at_branch_end {
                            // A whole-branch `**` matches anything.
                            current.push_str(".*");
                        } else if after_separator && at_branch_end {
                            // A trailing `/**` is globset's recursive suffix.
                            current.push_str(".*");
                        } else {
                            // globset treats `**` not on a component boundary
                            // as two ordinary stars: no separator crossing.
                            current.push_str("[^/]*");
                        }
                    } else {
                        current.push_str("[^/]*");
                    }
                }
                '?' => current.push_str("[^/]"),
                '[' => {
                    let (negated, ranges) = self.parse_class()?;
                    current.push_str(&render_class(negated, &ranges)?);
                }
                '/' => current.push('/'),
                ch => push_literal(&mut current, ch),
            }
        }
        if in_brace {
            return Err("the deny rule has an unclosed `{`".to_string());
        }
        if current.is_empty() {
            return Err("the deny rule is empty".to_string());
        }
        branches.push(current);
        Ok(branches)
    }

    /// Port of globset's class parser: a leading `!`/`^` negates, a leading `]`
    /// is a literal member, and `-` forms ranges unless it is first or last.
    fn parse_class(&mut self) -> Result<(bool, Vec<(char, char)>), String> {
        let negated = match self.chars.peek() {
            Some('!') | Some('^') => {
                self.chars.next();
                true
            }
            _ => false,
        };
        let mut ranges: Vec<(char, char)> = Vec::new();
        let mut first = true;
        let mut in_range = false;
        loop {
            let Some(ch) = self.chars.next() else {
                return Err("the deny rule has an unclosed `[`".to_string());
            };
            match ch {
                ']' => {
                    if first {
                        ranges.push((']', ']'));
                    } else {
                        break;
                    }
                }
                '-' => {
                    if first {
                        ranges.push(('-', '-'));
                    } else if in_range {
                        let Some(last) = ranges.last_mut() else {
                            return Err("invalid character class".to_string());
                        };
                        last.1 = '-';
                        if last.1 < last.0 {
                            return Err("invalid character class range".to_string());
                        }
                        in_range = false;
                    } else {
                        if ranges.is_empty() {
                            return Err("invalid character class".to_string());
                        }
                        in_range = true;
                    }
                }
                ch => {
                    if in_range {
                        let Some(last) = ranges.last_mut() else {
                            return Err("invalid character class".to_string());
                        };
                        last.1 = ch;
                        if last.1 < last.0 {
                            return Err("invalid character class range".to_string());
                        }
                        in_range = false;
                    } else {
                        ranges.push((ch, ch));
                    }
                }
            }
            first = false;
        }
        if in_range {
            ranges.push(('-', '-'));
        }
        if ranges.is_empty() {
            return Err("the deny rule has an empty character class".to_string());
        }
        Ok((negated, ranges))
    }
}

/// Render a glob class as a POSIX bracket expression.
///
/// Members are case-folded because the glob matcher is case-insensitive and
/// POSIX ERE has no inline `(?i)` flag. `-` and `]` are ordered so they stay
/// literal; `[`, `\` and `^` have no unambiguous bracket-expression spelling
/// here, so a class containing them fails the launch rather than silently
/// matching something else.
fn render_class(negated: bool, ranges: &[(char, char)]) -> Result<String, String> {
    let mut singles: Vec<char> = Vec::new();
    let mut ranges_out: Vec<(char, char)> = Vec::new();
    let mut add = |range: (char, char)| {
        if range.0 == range.1 {
            singles.push(range.0);
        } else {
            ranges_out.push(range);
        }
    };
    for &(low, high) in ranges {
        add((low, high));
        // Add the opposite case for the letter portion(s) of the range.
        let lower_low = low.max('a');
        let lower_high = high.min('z');
        if lower_low <= lower_high {
            add((
                lower_low.to_ascii_uppercase(),
                lower_high.to_ascii_uppercase(),
            ));
        }
        let upper_low = low.max('A');
        let upper_high = high.min('Z');
        if upper_low <= upper_high {
            add((
                upper_low.to_ascii_lowercase(),
                upper_high.to_ascii_lowercase(),
            ));
        }
    }
    for (low, high) in &ranges_out {
        for endpoint in [low, high] {
            if matches!(endpoint, '[' | '\\' | '^' | ']' | '-') {
                return Err(format!(
                    "the deny rule character class contains `{endpoint}`, which cannot be represented safely"
                ));
            }
        }
    }
    singles.sort_unstable();
    singles.dedup();
    if singles.iter().any(|ch| matches!(ch, '[' | '\\' | '^')) {
        return Err(
            "the deny rule character class contains a character POSIX ERE cannot express here"
                .to_string(),
        );
    }
    let leading_close = singles.first() == Some(&']');
    if leading_close {
        singles.remove(0);
    }
    let trailing_dash = singles.last() == Some(&'-');
    if trailing_dash {
        singles.pop();
    }
    ranges_out.sort_unstable();
    ranges_out.dedup();
    let mut out = String::from("[");
    if negated {
        out.push('^');
    }
    if leading_close {
        out.push(']');
    }
    for (low, high) in &ranges_out {
        out.push(*low);
        out.push('-');
        out.push(*high);
    }
    for ch in &singles {
        out.push(*ch);
    }
    if trailing_dash {
        out.push('-');
    }
    out.push(']');
    Ok(out)
}

fn push_literal(out: &mut String, ch: char) {
    if ch.is_ascii_alphabetic() {
        out.push('[');
        out.push(ch.to_ascii_lowercase());
        out.push(ch.to_ascii_uppercase());
        out.push(']');
    } else {
        if "\\.[]{}()+*?^$|".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
}
