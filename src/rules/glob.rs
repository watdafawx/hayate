//! Case-insensitive glob: `*` = any run of chars (crosses `\`), `?` = one char.
//! Used for rule patterns and Watch's dev-build ignore list.

fn fold(c: char) -> char {
    if c == '/' {
        '\\'
    } else if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

pub fn is_glob(p: &str) -> bool {
    p.contains(['*', '?'])
}

pub fn matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().map(fold).collect();
    let t: Vec<char> = text.chars().map(fold).collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert!(matches("*.exe", "C:\\a\\b.EXE"));
        assert!(matches("X:\\Games\\*\\*.exe", "x:\\games\\Foo\\bin\\foo.exe"));
        assert!(!matches("X:\\Games\\*\\*.exe", "x:\\other\\foo.exe"));
        assert!(matches("*\\target\\debug\\*", "X:\\Dev\\hayate\\target\\debug\\hayate.exe"));
        assert!(matches("*\\target\\*\\deps\\*", "X:\\Dev\\h\\target\\release\\deps\\h-0a1b2c.exe"));
        assert!(!matches("*\\target\\debug\\*", "X:\\Dev\\hayate\\target\\release\\hayate.exe"));
        assert!(matches("setup?.exe", "SETUP1.exe"));
        assert!(!matches("setup?.exe", "setup.exe"));
        assert!(matches("*", ""));
        assert!(matches("a*b*c", "aXXbYYc"));
        assert!(!matches("a*b*c", "aXXbYY"));
        assert!(matches("c:/windows/*", "C:\\Windows\\x.exe"));
    }
}
