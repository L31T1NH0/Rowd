#[derive(Default, Clone)]
pub struct Ignore(Vec<String>);
impl Ignore {
    pub fn parse(text: &str) -> Self {
        Self(
            text.lines()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.starts_with('#'))
                .map(str::to_owned)
                .collect(),
        )
    }
    pub fn matches(&self, path: &str, directory: bool) -> bool {
        if path.split('/').any(|s| s == ".rowd") || path == ".rowdignore" {
            return true;
        }
        self.0.iter().any(|pattern| {
            let dir_only = pattern.ends_with('/');
            let pattern = pattern.trim_end_matches('/').trim_start_matches('/');
            let parts: Vec<_> = path.split('/').collect();
            (1..=parts.len()).any(|n| {
                if dir_only && n == parts.len() && !directory {
                    return false;
                }
                let candidate = if pattern.contains('/') {
                    parts[..n].join("/")
                } else {
                    parts[n - 1].to_string()
                };
                wildcard(pattern.as_bytes(), candidate.as_bytes())
            })
        })
    }
}
fn wildcard(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t, mut star, mut retry) = (0, 0, None, 0);
    while t < text.len() {
        if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = t;
        } else if let Some(s) = star {
            retry += 1;
            t = retry;
            p = s + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patterns() {
        let i = Ignore::parse("# comment\n\nnode_modules/\n*.tmp\ndocs/private/\n.git/\n");
        for p in [
            "x/node_modules/a",
            "a.tmp",
            "x/a.tmp",
            "docs/private/a",
            ".git/config",
            ".rowdignore",
        ] {
            assert!(i.matches(p, false), "{p}");
        }
        for p in ["node_modules", "docs/public/a", "a.tmp.txt"] {
            assert!(!i.matches(p, false), "{p}");
        }
    }
}
