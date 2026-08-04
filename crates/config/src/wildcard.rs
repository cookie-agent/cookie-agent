/// `*` matches any characters including `/`; `?` matches exactly one character.
#[must_use]
pub fn simple_wildcard_match(pattern: &str, resource: &str) -> bool {
    wildcard_match(pattern, resource)
        || pattern
            .strip_suffix(" *")
            .is_some_and(|prefix| wildcard_match(prefix, resource))
}
fn wildcard_match(pattern: &str, resource: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let resource = resource.chars().collect::<Vec<_>>();
    let (mut p, mut r, mut star, mut retry) = (0, 0, None, 0);
    while r < resource.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == resource[r]) {
            p += 1;
            r += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            p += 1;
            retry = r;
        } else if let Some(index) = star {
            p = index + 1;
            retry += 1;
            r = retry;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}
