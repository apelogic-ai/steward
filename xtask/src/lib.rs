use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
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

    for region in text_regions(content) {
        for token in region.split(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, '@' | '.' | ':' | '-' | '_')
        }) {
            if token.is_empty() {
                continue;
            }

            if token.contains('@') {
                if !is_reserved_email(token) {
                    violations.push(format!("non-reserved email: {token}"));
                }
                continue;
            }

            if let Ok(address) = token.parse::<IpAddr>() {
                match address {
                    IpAddr::V4(address) if !is_documentation_ipv4(address) => {
                        violations.push(format!("non-reserved IPv4 address: {token}"));
                    }
                    IpAddr::V6(address) if !is_documentation_ipv6(address) => {
                        violations.push(format!("non-reserved IPv6 address: {token}"));
                    }
                    IpAddr::V4(_) | IpAddr::V6(_) => {}
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
            let has_connection_credential = contains_password_in_url(line);

            (has_key
                || has_github_token
                || has_provider_key
                || has_aws_key
                || has_credential_assignment
                || has_connection_credential)
                .then_some(index + 1)
        })
        .collect()
}

pub fn validate_register_content(content: &str) -> Result<(), String> {
    let root = toml::from_str::<toml::Table>(content)
        .map_err(|error| format!("conformance register is not valid TOML: {error}"))?;

    if root.get("schema").and_then(toml::Value::as_integer) != Some(1) {
        return Err("conformance register must declare schema = 1".to_owned());
    }
    if root
        .get("meta")
        .and_then(toml::Value::as_table)
        .and_then(|meta| meta.get("pinned_openshell"))
        .and_then(toml::Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("conformance register must pin OpenShell in one place".to_owned());
    }
    if root.contains_key("status")
        || root
            .values()
            .any(|value| contains_toml_key(value, "status"))
    {
        return Err("conformance register must not contain a hand-authored status".to_owned());
    }

    let guarantees = root
        .get("guarantee")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "conformance register must contain guarantee entries".to_owned())?;
    let ids = guarantees
        .iter()
        .filter_map(|guarantee| guarantee.get("id").and_then(toml::Value::as_str))
        .collect::<Vec<_>>();
    let unique = ids.iter().copied().collect::<BTreeSet<_>>();
    if ids.len() != 6 || unique.len() != ids.len() {
        return Err("conformance register must contain unique entries G-1 through G-6".to_owned());
    }
    for expected in 1..=6 {
        let id = format!("G-{expected}");
        if !unique.contains(id.as_str()) {
            return Err(format!("conformance register is missing {id}"));
        }
    }
    Ok(())
}

pub fn migration_history_violations(changes: &str) -> Vec<String> {
    changes
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let status = fields.next()?;
            if status == "A" {
                return None;
            }
            let paths = fields.collect::<Vec<_>>().join(" -> ");
            Some(format!("{status} {paths}"))
        })
        .collect()
}

fn contains_toml_key(value: &toml::Value, key: &str) -> bool {
    match value {
        toml::Value::Table(table) => {
            table.contains_key(key) || table.values().any(|value| contains_toml_key(value, key))
        }
        toml::Value::Array(values) => values.iter().any(|value| contains_toml_key(value, key)),
        _ => false,
    }
}

fn text_regions(content: &str) -> Vec<String> {
    let characters = content.chars().collect::<Vec<_>>();
    let mut regions = Vec::new();
    let mut index = 0;

    while index < characters.len() {
        if matches!(characters[index], '"' | '\'') {
            let delimiter = characters[index];
            index += 1;
            let mut region = String::new();
            let mut escaped = false;
            let mut closed = false;
            while index < characters.len() {
                let character = characters[index];
                index += 1;
                if escaped {
                    region.push(character);
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == delimiter {
                    closed = true;
                    break;
                } else {
                    region.push(character);
                }
            }
            if closed {
                regions.push(region);
            }
            continue;
        }

        if characters[index] == '/' && characters.get(index + 1) == Some(&'/') {
            index += 2;
            let mut region = String::new();
            while index < characters.len() && characters[index] != '\n' {
                region.push(characters[index]);
                index += 1;
            }
            regions.push(region);
            continue;
        }

        if characters[index] == '/' && characters.get(index + 1) == Some(&'*') {
            index += 2;
            let mut depth = 1;
            let mut region = String::new();
            while index < characters.len() && depth > 0 {
                if characters[index] == '/' && characters.get(index + 1) == Some(&'*') {
                    depth += 1;
                    index += 2;
                } else if characters[index] == '*' && characters.get(index + 1) == Some(&'/') {
                    depth -= 1;
                    index += 2;
                } else {
                    region.push(characters[index]);
                    index += 1;
                }
            }
            regions.push(region);
            continue;
        }

        index += 1;
    }

    regions
}

fn is_reserved_email(token: &str) -> bool {
    let Some((local, domain)) = token.rsplit_once('@') else {
        return false;
    };
    !local.is_empty() && !local.contains('@') && is_example_domain(domain)
}

fn is_documentation_ipv4(address: Ipv4Addr) -> bool {
    matches!(
        address.octets(),
        [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _]
    )
}

fn is_documentation_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    segments[0] == 0x2001 && segments[1] == 0x0db8
}

fn looks_like_hostname(token: &str) -> bool {
    let labels = token.split('.').collect::<Vec<_>>();
    let Some(top_level) = labels.last() else {
        return false;
    };
    labels.len() >= 2
        && top_level.len() >= 2
        && top_level.bytes().all(|byte| byte.is_ascii_alphabetic())
        && labels.iter().all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn is_reserved_hostname(token: &str) -> bool {
    token == "test" || token.ends_with(".test") || is_example_domain(token)
}

fn is_example_domain(token: &str) -> bool {
    token == "example.com"
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
    let compact = line
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    let marker = ["pass", "word="].concat();
    let Some(index) = compact.find(&marker) else {
        return false;
    };
    let value = &compact[index + marker.len()..];
    is_literal_secret(value)
}

fn contains_password_in_url(line: &str) -> bool {
    line.split_whitespace().any(|raw| {
        let token = raw.trim_matches(|character: char| {
            matches!(character, '"' | '\'' | '(' | ')' | ',' | ';')
        });
        let Some((_scheme, remainder)) = token.split_once("://") else {
            return false;
        };
        let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
        let Some((user_info, _host)) = authority.rsplit_once('@') else {
            return false;
        };
        let Some((_user, password)) = user_info.split_once(':') else {
            return false;
        };
        is_literal_secret(password)
    })
}

fn is_literal_secret(value: &str) -> bool {
    let value = value.trim_start_matches(['"', '\'']);
    !value.is_empty()
        && !value.starts_with('<')
        && !value.starts_with("${")
        && !value.starts_with("env:")
}

#[cfg(test)]
mod tests {
    use super::{
        local_test_context_is_safe, migration_history_violations, neutrality_violations,
        secret_violations, validate_register_content,
    };
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
    fn neutrality_ignores_dotted_non_hostnames() {
        let violations = neutrality_violations("\"v1.2.3\" \"schema.v2\"");

        assert!(
            violations.is_empty(),
            "version strings and dotted identifiers are not hostnames: {violations:?}"
        );
    }

    #[test]
    fn neutrality_ignores_dotted_code_selectors() {
        let violations = neutrality_violations("let names = PORTS.iter().map(|port| port.name);");

        assert!(
            violations.is_empty(),
            "code selectors outside strings and comments are not hostnames: {violations:?}"
        );
    }

    #[test]
    fn neutrality_allows_reserved_email_subdomains() {
        let violations = neutrality_violations("\"alice@team-a.example.com\"");

        assert!(
            violations.is_empty(),
            "email domains beneath reserved example domains must be allowed: {violations:?}"
        );
    }

    #[test]
    fn neutrality_rejects_non_reserved_ipv6() {
        let address = ["fe80", "", "1ff", "fe23", "4567", "890a"].join(":");
        let violations = neutrality_violations(&format!("\"{address}\""));

        assert_eq!(
            violations.len(),
            1,
            "neutrality gate must reject a non-documentation IPv6 address"
        );
    }

    #[test]
    fn neutrality_allows_documentation_ipv6() {
        let violations = neutrality_violations("\"2001:db8::1\"");

        assert!(
            violations.is_empty(),
            "the RFC 3849 documentation prefix must remain allowed: {violations:?}"
        );
    }

    #[test]
    fn neutrality_scans_comments() {
        let email = ["ops", "corp.invalid"].join("@");
        let ip = ["10", "0", "0", "5"].join(".");
        let violations = neutrality_violations(&format!("// reach {email} at {ip}"));

        assert_eq!(
            violations.len(),
            2,
            "neutrality gate must inspect identifiers in comments"
        );
    }

    #[test]
    fn neutrality_scans_single_quoted_text() {
        let host = ["service", "corp.invalid"].join(".");
        let violations = neutrality_violations(&format!("'{host}'"));

        assert_eq!(
            violations.len(),
            1,
            "neutrality gate must inspect identifiers in single-quoted fixture text"
        );
    }

    #[test]
    fn secret_scan_allows_quoted_environment_reference() {
        let content = br#"password = "${DB_PASSWORD}""#;

        let violations = secret_violations(Path::new("config.toml"), content);

        assert!(
            violations.is_empty(),
            "quoted environment references must not be reported as credentials"
        );
    }

    #[test]
    fn secret_scan_rejects_password_in_connection_url() {
        let scheme = ["post", "gres"].concat();
        let separator = [":", "//"].concat();
        let user_info = ["alice", "not-a-secret"].join(":");
        let content = format!("{scheme}{separator}{user_info}@db.example.com/example");

        let violations = secret_violations(Path::new("config.toml"), content.as_bytes());

        assert_eq!(
            violations,
            vec![1],
            "credential-bearing connection URLs must be rejected"
        );
    }

    #[test]
    fn secret_scan_allows_environment_password_in_connection_url() {
        let scheme = ["post", "gres"].concat();
        let separator = [":", "//"].concat();
        let user_info = ["alice", "${DB_PASSWORD}"].join(":");
        let content = format!("{scheme}{separator}{user_info}@db.example.com/example");

        let violations = secret_violations(Path::new("config.toml"), content.as_bytes());

        assert!(
            violations.is_empty(),
            "environment-backed URL passwords must not be reported as credentials"
        );
    }

    #[test]
    fn register_ignores_assignment_shaped_prose() {
        let content = r#"
schema = 1

[meta]
pinned_openshell = "v0.0.82"

[[guarantee]]
id = "G-1"
watch = """
status = holds only after S5
id = "G-3"
"""

[[guarantee]]
id = "G-2"

[[guarantee]]
id = "G-3"

[[guarantee]]
id = "G-4"

[[guarantee]]
id = "G-5"

[[guarantee]]
id = "G-6"
"#;

        let validation = validate_register_content(content);

        assert!(
            validation.is_ok(),
            "assignment-shaped prose inside a TOML string must not affect register structure: {validation:?}"
        );
    }

    #[test]
    fn register_rejects_structural_status() {
        let mut guarantees = (1..=6)
            .map(|id| format!("[[guarantee]]\nid = \"G-{id}\"\n"))
            .collect::<String>();
        guarantees.push_str("status = \"holds\"\n");
        let content = format!("schema = 1\n[meta]\npinned_openshell = \"v0.0.82\"\n{guarantees}");

        let validation = validate_register_content(&content);

        assert_eq!(
            validation,
            Err("conformance register must not contain a hand-authored status".to_owned()),
            "a structural status key must remain forbidden"
        );
    }

    #[test]
    fn migration_history_rejects_modified_files() {
        let changes = "M\tmigrations/0001_initial.sql\n";

        let violations = migration_history_violations(changes);

        assert_eq!(
            violations,
            vec!["M migrations/0001_initial.sql"],
            "an existing migration modification must fail the append-only check"
        );
    }

    #[test]
    fn migration_history_allows_new_files() {
        let changes = "A\tmigrations/0001_initial.sql\n";

        let violations = migration_history_violations(changes);

        assert!(
            violations.is_empty(),
            "new migration files must remain allowed: {violations:?}"
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
