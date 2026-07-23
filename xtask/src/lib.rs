use std::path::Path;

pub fn local_test_context_is_safe(context: &str) -> bool {
    ["kind-steward-", "k3d-steward-"]
        .into_iter()
        .find_map(|prefix| context.strip_prefix(prefix))
        .is_some_and(|run_id| {
            !run_id.is_empty()
                && run_id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

pub fn neutrality_violations(content: &str) -> Vec<String> {
    let mut violations = Vec::new();

    for literal in string_literals(content) {
        for raw in literal.split_whitespace() {
            let token = raw.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric()
                    && !matches!(character, '@' | '.' | ':' | '-' | '_')
            });

            if token.contains('@') {
                let allowed = token.ends_with("@example.com") || token.ends_with("@example.org");
                if !allowed {
                    violations.push(format!("non-reserved email: {token}"));
                }
                continue;
            }

            if is_ipv4(token) {
                if !is_documentation_ipv4(token) {
                    violations.push(format!("non-reserved IPv4 address: {token}"));
                }
                continue;
            }

            if looks_like_hostname(token) && !is_reserved_hostname(token) {
                violations.push(format!("non-reserved hostname: {token}"));
            }
        }
    }

    violations
}

pub fn secret_violations(path: &Path, content: &[u8]) -> Vec<usize> {
    if is_sensitive_path(path) {
        return vec![1];
    }

    if content.contains(&0) {
        return Vec::new();
    }

    let Ok(text) = std::str::from_utf8(content) else {
        return Vec::new();
    };
    let private_key_marker = ["BEGIN", "PRIVATE", "KEY"].join(" ");
    let rsa_key_marker = ["BEGIN", "RSA", "PRIVATE", "KEY"].join(" ");
    let openssh_key_marker = ["BEGIN", "OPENSSH", "PRIVATE", "KEY"].join(" ");
    let github_prefixes = [
        ["gh", "p_"].concat(),
        ["gh", "o_"].concat(),
        ["gh", "u_"].concat(),
        ["gh", "s_"].concat(),
        ["gh", "r_"].concat(),
    ];
    let provider_prefix = ["s", "k-"].concat();

    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let has_key = line.contains(&private_key_marker)
                || line.contains(&rsa_key_marker)
                || line.contains(&openssh_key_marker);
            let has_github_token = github_prefixes
                .iter()
                .any(|prefix| contains_prefixed_secret(line, prefix, 20));
            let has_provider_key = contains_prefixed_secret(line, &provider_prefix, 20);
            let has_aws_key = contains_aws_access_key(line);
            let has_credential_assignment = contains_password_assignment(line);

            (has_key
                || has_github_token
                || has_provider_key
                || has_aws_key
                || has_credential_assignment)
                .then_some(index + 1)
        })
        .collect()
}

fn is_ipv4(token: &str) -> bool {
    let parts = token.split('.').collect::<Vec<_>>();
    parts.len() == 4
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.parse::<u8>().is_ok())
}

fn string_literals(content: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut current = String::new();
    let mut in_literal = false;
    let mut escaped = false;

    for character in content.chars() {
        if in_literal {
            if escaped {
                current.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                literals.push(std::mem::take(&mut current));
                in_literal = false;
            } else {
                current.push(character);
            }
        } else if character == '"' {
            in_literal = true;
        }
    }

    literals
}

fn is_documentation_ipv4(token: &str) -> bool {
    token.starts_with("192.0.2.")
        || token.starts_with("198.51.100.")
        || token.starts_with("203.0.113.")
}

fn looks_like_hostname(token: &str) -> bool {
    token.contains('.')
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        && token.bytes().any(|byte| byte.is_ascii_alphabetic())
}

fn is_reserved_hostname(token: &str) -> bool {
    token == "test"
        || token.ends_with(".test")
        || token == "example.com"
        || token.ends_with(".example.com")
        || token == "example.org"
        || token.ends_with(".example.org")
}

fn is_sensitive_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let extension = path.extension().and_then(|extension| extension.to_str());

    name == ".env"
        || name.starts_with(".env.")
        || name == "kubeconfig"
        || name.starts_with("kubeconfig.")
        || matches!(extension, Some("pem" | "key" | "p12" | "pfx"))
        || (name.ends_with(".jwks.json") && !name.ends_with(".pub.jwks.json"))
}

fn contains_prefixed_secret(line: &str, prefix: &str, minimum_suffix: usize) -> bool {
    line.match_indices(prefix).any(|(index, _)| {
        line[index + prefix.len()..]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .count()
            >= minimum_suffix
    })
}

fn contains_aws_access_key(line: &str) -> bool {
    let prefix = ["AK", "IA"].concat();
    line.match_indices(&prefix).any(|(index, _)| {
        let suffix = &line[index + prefix.len()..];
        suffix
            .chars()
            .take_while(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
            .count()
            >= 16
    })
}

fn contains_password_assignment(line: &str) -> bool {
    let compact = line.replace(' ', "").to_ascii_lowercase();
    let marker = ["pass", "word="].concat();
    let Some(index) = compact.find(&marker) else {
        return false;
    };
    let value = &compact[index + marker.len()..];
    !value.is_empty()
        && !value.starts_with('<')
        && !value.starts_with("${")
        && !value.starts_with("env:")
}

#[cfg(test)]
mod tests {
    use super::{local_test_context_is_safe, neutrality_violations, secret_violations};
    use std::path::Path;

    #[test]
    fn neutrality_rejects_non_reserved_identifiers() {
        let email = ["alice", "corp.invalid"].join("@");
        let host = ["service", "corp.invalid"].join(".");
        let ip = ["203", "0", "114", "9"].join(".");
        let content = format!("\"{email}\" \"{host}\" \"{ip}\"");

        let violations = neutrality_violations(&content);

        assert_eq!(
            violations.len(),
            3,
            "neutrality gate must reject every non-reserved identifier"
        );
    }

    #[test]
    fn secret_scan_rejects_private_key_material_without_echoing_it() {
        let marker = ["BEGIN", "PRIVATE", "KEY"].join(" ");
        let content = format!("prefix {marker} suffix");

        let violations = secret_violations(Path::new("fixture.txt"), content.as_bytes());

        assert_eq!(
            violations,
            vec![1],
            "secret gate must report only the line number containing key material"
        );
    }

    #[test]
    fn local_context_requires_a_run_id_after_the_reserved_prefix() {
        assert!(
            !local_test_context_is_safe("kind-steward-"),
            "a local context without a run ID must be rejected"
        );
    }
}
