/// Substrings of an assignment's name that mark its value as a credential.
const SECRET_NAMES: [&str; 9] = [
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "access_key",
    "private_key",
];

fn name_end(text: &str) -> usize {
    text.bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || b"_-".contains(b))
        .count()
}

fn next_name(text: &str) -> Option<usize> {
    let (start, length) = SECRET_NAMES
        .iter()
        .filter_map(|name| text.find(name).map(|start| (start, name.len())))
        .min()?;
    let suffix = start + length;
    Some(suffix + name_end(&text[suffix..]))
}

pub(super) fn redact(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let mut out = String::with_capacity(line.len());
    let mut at = 0;
    while let Some(length) = next_name(&lower[at..]) {
        let name_end = at + length;
        let rest = &line[name_end..];
        let separator = rest.trim_start_matches(['"', '\'', ' ', '\t']);
        let Some(value) = separator.strip_prefix(['=', ':']) else {
            out.push_str(&line[at..name_end]);
            at = name_end;
            continue;
        };
        let value = value.trim_start_matches([' ', '\t', '"', '\'']);
        let start = line.len() - value.len();
        let value = value
            .split([' ', '\t', '"', '\'', '&', ',', ';', ')'])
            .next()
            .unwrap_or_default();
        out.push_str(&line[at..start]);
        if !value.is_empty() {
            out.push_str(super::REDACTED);
        }
        at = start + value.len();
    }
    out.push_str(&line[at..]);
    out
}
