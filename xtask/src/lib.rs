use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestOutcome {
    Passed,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DerivedStatus {
    Provided,
    Partial,
    NotYetProvided,
    Regressed,
    GapMayHaveClosed,
    Unevidenced,
}

pub fn derive_status(
    has_declared_gaps: bool,
    holds: &[TestOutcome],
    gaps: &[TestOutcome],
) -> DerivedStatus {
    if holds.contains(&TestOutcome::Failed) {
        return DerivedStatus::Regressed;
    }
    if gaps.contains(&TestOutcome::Failed) {
        return DerivedStatus::GapMayHaveClosed;
    }
    if holds.is_empty() && gaps.is_empty() {
        return DerivedStatus::Unevidenced;
    }
    if holds.is_empty() {
        return DerivedStatus::NotYetProvided;
    }
    if has_declared_gaps {
        return DerivedStatus::Partial;
    }
    DerivedStatus::Provided
}

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
                if is_globally_routable(address) {
                    match address {
                        IpAddr::V4(_) => {
                            violations.push(format!("non-reserved IPv4 address: {token}"));
                        }
                        IpAddr::V6(_) => {
                            violations.push(format!("non-reserved IPv6 address: {token}"));
                        }
                    }
                }
                continue;
            }

            if looks_like_hostname(token)
                && !is_allowed_filename(token)
                && !is_reserved_hostname(token)
            {
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
            let has_provider_key = contains_provider_key(line, &provider_prefix);
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
    let meta = root
        .get("meta")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "conformance register must declare environment provenance".to_owned())?;
    for (field, dependency) in [
        ("pinned_openshell", "OpenShell"),
        ("pinned_spire", "SPIRE"),
        ("pinned_litellm", "LiteLLM"),
        ("pinned_agent_sandbox", "Agent Sandbox"),
        ("pinned_mcp_gw", "mcp-gw"),
    ] {
        if meta
            .get(field)
            .and_then(toml::Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(format!(
                "conformance register must pin {dependency} in one place"
            ));
        }
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

pub fn migration_base_candidates(configured: Option<&str>) -> Vec<String> {
    let configured = configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|value| !value.bytes().all(|byte| byte == b'0'));

    configured.map_or_else(
        || vec!["main".to_owned(), "origin/main".to_owned()],
        |value| vec![value.to_owned()],
    )
}

pub fn select_migration_base(
    candidates: &[String],
    resolved: &[(String, String)],
) -> Result<String, String> {
    if resolved.is_empty() {
        return Err(if candidates.len() == 1 {
            format!(
                "migration comparison base {} does not resolve to a commit",
                candidates[0]
            )
        } else {
            "no migration comparison base is available; set STEWARD_MIGRATION_BASE or sync local main"
                .to_owned()
        });
    }

    resolved
        .first()
        .map(|(reference, _commit)| reference.clone())
        .ok_or_else(|| "no migration comparison base is available".to_owned())
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
        if characters[index] == '\'' && is_rust_lifetime(&characters, index) {
            index += 1;
            continue;
        }

        if matches!(characters[index], '"' | '\'') {
            let opening = index;
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
            } else {
                index = opening + 1;
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

fn is_rust_lifetime(characters: &[char], quote: usize) -> bool {
    let Some(first) = characters.get(quote + 1) else {
        return false;
    };
    if !first.is_ascii_alphabetic() && *first != '_' {
        return false;
    }

    let mut after = quote + 2;
    while characters
        .get(after)
        .is_some_and(|character| character.is_ascii_alphanumeric() || *character == '_')
    {
        after += 1;
    }
    if characters.get(after) == Some(&'\'') {
        return false;
    }

    matches!(
        characters.get(after),
        Some('>' | ',' | ':' | '+' | ')' | ']')
    ) || characters
        .get(after)
        .is_some_and(|character| character.is_whitespace())
        && characters[..quote]
            .iter()
            .rev()
            .find(|character| !character.is_whitespace())
            .is_some_and(|character| matches!(character, '<' | '&' | ':' | '+' | ','))
}

fn is_reserved_email(token: &str) -> bool {
    let Some((local, domain)) = token.rsplit_once('@') else {
        return false;
    };
    !local.is_empty() && !local.contains('@') && is_example_domain(domain)
}

fn is_globally_routable(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_globally_routable_ipv4(address),
        IpAddr::V6(address) => is_globally_routable_ipv6(address),
    }
}

fn is_globally_routable_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    let is_shared = octets[0] == 100 && octets[1] & 0b1100_0000 == 0b0100_0000;
    let is_protocol_assignment = matches!(octets, [192, 0, 0, last] if last != 9 && last != 10);
    let is_benchmarking = octets[0] == 198 && octets[1] & 0xfe == 18;
    let is_reserved = octets[0] & 0xf0 == 0xf0 && address != Ipv4Addr::BROADCAST;

    !(octets[0] == 0
        || address.is_private()
        || is_shared
        || address.is_loopback()
        || address.is_link_local()
        || is_protocol_assignment
        || address.is_documentation()
        || is_benchmarking
        || is_reserved
        || address == Ipv4Addr::BROADCAST)
}

fn is_globally_routable_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    let value = u128::from_be_bytes(address.octets());
    let is_protocol_assignment = segments[0] == 0x2001 && segments[1] < 0x200;
    let is_protocol_assignment_exception = matches!(
        value,
        0x2001_0001_0000_0000_0000_0000_0000_0001 | 0x2001_0001_0000_0000_0000_0000_0000_0002
    ) || segments[0] == 0x2001
        && (segments[1] == 3
            || segments[1] == 4 && segments[2] == 0x112
            || (0x20..=0x3f).contains(&segments[1]));
    let is_documentation = (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] == 0x3fff && segments[1] & 0xf000 == 0);

    !(address.is_unspecified()
        || address.is_loopback()
        || matches!(segments, [0, 0, 0, 0, 0, 0xffff, _, _])
        || matches!(segments, [0x64, 0xff9b, 1, _, _, _, _, _])
        || matches!(segments, [0x100, 0, 0, 0, _, _, _, _])
        || is_protocol_assignment && !is_protocol_assignment_exception
        || matches!(segments, [0x2002, _, _, _, _, _, _, _])
        || is_documentation
        || matches!(segments, [0x5f00, ..])
        || address.is_unique_local()
        || address.is_unicast_link_local())
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

// Dotted tokens are ambiguous even when they appear inside paths because the
// neutrality tokenizer deliberately discards path context. Keep this list exact
// and security-biased; add a narrowly reviewed entry when a fixture must name
// another file so unknown hostname-shaped tokens continue to fail closed.
fn is_allowed_filename(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "build.rs"
            | "changelog.md"
            | "config.toml"
            | "config.yaml"
            | "config.yml"
            | "contributing.md"
            | "fixture.txt"
            | "jwks.json"
            | "lib.rs"
            | "license.md"
            | "main.rs"
            | "main.txt"
            | "mod.rs"
            | "readme.md"
    )
}

fn is_reserved_hostname(token: &str) -> bool {
    token == "test"
        || token.ends_with(".test")
        || is_example_domain(token)
        || is_recognized_upstream_hostname(token)
}

fn is_example_domain(token: &str) -> bool {
    token == "example.com"
        || token.ends_with(".example.com")
        || token == "example.org"
        || token.ends_with(".example.org")
}

fn is_recognized_upstream_hostname(token: &str) -> bool {
    ["crates.io", "docs.rs", "github.com", "openpolicyagent.org"]
        .into_iter()
        .any(|domain| token == domain || token.ends_with(&format!(".{domain}")))
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
        || ((name == "jwks.json" || name.ends_with(".jwks.json"))
            && !name.ends_with(".pub.jwks.json"))
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

fn contains_provider_key(line: &str, prefix: &str) -> bool {
    line.match_indices(prefix).any(|(index, _)| {
        let has_token_boundary = line[..index].chars().next_back().is_none_or(|character| {
            !character.is_ascii_alphanumeric() && !matches!(character, '_' | '-')
        });
        if !has_token_boundary {
            return false;
        }

        let suffix = &line[index + prefix.len()..];
        // Legacy keys have no class tag, so a same-shaped identifier cannot be
        // distinguished safely. Keep this branch conservative; structured keys
        // are handled separately below without broadening the legacy shape.
        let legacy_payload_length = suffix
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .count();
        if legacy_payload_length >= 20 {
            return true;
        }

        provider_payload(suffix).is_some_and(|payload| {
            payload
                .chars()
                .take_while(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                })
                .count()
                >= 20
        })
    })
}

fn provider_payload(suffix: &str) -> Option<&str> {
    if let Some(payload) = suffix
        .strip_prefix("proj-")
        .or_else(|| suffix.strip_prefix("svcacct-"))
    {
        return Some(payload);
    }

    let versioned = suffix.strip_prefix("ant-api")?;
    let (version, payload) = versioned.split_once('-')?;
    (version.len() == 2 && version.bytes().all(|byte| byte.is_ascii_digit())).then_some(payload)
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
        DerivedStatus, TestOutcome, derive_status, local_test_context_is_safe,
        migration_base_candidates, migration_history_violations, neutrality_violations,
        secret_violations, select_migration_base, validate_register_content,
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
    fn neutrality_rejects_globally_routable_ipv6() {
        let address = ["2001", "db9", "", "1"].join(":");
        let violations = neutrality_violations(&format!("\"{address}\""));

        assert_eq!(
            violations.len(),
            1,
            "neutrality gate must reject a globally routable IPv6 address"
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
    fn neutrality_allows_non_global_ip_addresses() {
        let violations = neutrality_violations(
            "\"127.0.0.1\" \"0.0.0.0\" \"10.0.0.1\" \"192.168.1.1\" \
             \"169.254.1.1\" \"::1\" \"::\" \"fc00::1\" \"fe80::1\"",
        );

        assert!(
            violations.is_empty(),
            "every non-globally-routable IP address must be allowed: {violations:?}"
        );
    }

    #[test]
    fn neutrality_scans_comments() {
        let email = ["ops", "corp.invalid"].join("@");
        let ip = ["203", "0", "114", "5"].join(".");
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
    fn neutrality_scans_after_rust_lifetimes() {
        let host = ["secret", "corp.invalid"].join(".");
        let content = format!("fn borrow<'a>() {{}} let host = \"{host}\";");

        let violations = neutrality_violations(&content);

        assert_eq!(
            violations.len(),
            1,
            "a Rust lifetime must not blind the neutrality gate to later identifiers"
        );
    }

    #[test]
    fn neutrality_ignores_common_filenames() {
        let violations = neutrality_violations("\"src/main.rs\" \"config.yaml\" \"README.md\"");

        assert!(
            violations.is_empty(),
            "routine filename literals are not hostnames: {violations:?}"
        );
    }

    #[test]
    fn neutrality_still_rejects_hostname_shaped_like_a_filename() {
        let hostname = ["service", "rs"].join(".");
        let violations = neutrality_violations(&format!("\"{hostname}\""));

        assert_eq!(
            violations.len(),
            1,
            "an ambiguous bare name under a real top-level domain must remain a hostname"
        );
    }

    #[test]
    fn neutrality_rejects_extension_shaped_hostnames() {
        let host = ["svc", "go"].join(".");
        let basename = ["deploy", "sh"].join(".");
        let path = ["scripts", &basename].join("/");
        let violations = neutrality_violations(&format!("\"{host}\" \"{path}\""));

        assert_eq!(
            violations.len(),
            2,
            "filename extensions must not exempt hostname-shaped identifiers"
        );
    }

    #[test]
    fn neutrality_allows_recognized_upstream_hosts() {
        let violations = neutrality_violations(
            "// see github.com/org/repo, crates.io, docs.rs, and openpolicyagent.org",
        );

        assert!(
            violations.is_empty(),
            "recognized public upstream references must remain allowed in comments: {violations:?}"
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
    fn secret_scan_rejects_modern_dashed_provider_keys() {
        let prefix = ["s", "k"].concat();
        let payloads = [
            "abcde-fghijklmnopqrstuvwxyz012345",
            "abc_defghijklmnopqrstuvwxyz012345",
            "ab-cd_efghijklmnopqrstuvwxyz012345",
        ];
        let content = ["proj", "ant-api03", "svcacct"]
            .into_iter()
            .zip(payloads)
            .map(|(kind, payload)| [prefix.as_str(), kind, payload].join("-"))
            .collect::<Vec<_>>()
            .join("\n");

        let violations = secret_violations(Path::new("fixture.txt"), content.as_bytes());

        assert_eq!(
            violations,
            vec![1, 2, 3],
            "modern dashed provider-key formats must be rejected"
        );
    }

    #[test]
    fn secret_scan_rejects_future_dashed_provider_classes() {
        let prefix = ["s", "k"].concat();
        let payload = "abc-de_fghijklmnopqrstuvwxyz012345";
        let content = [prefix.as_str(), "ant-api04", payload].join("-");

        let violations = secret_violations(Path::new("fixture.txt"), content.as_bytes());

        assert_eq!(
            violations,
            vec![1],
            "future provider classes with base64url payloads must be rejected"
        );
    }

    #[test]
    fn secret_scan_allows_ordinary_kebab_case_text() {
        let content = [
            "disk-usage-monitoring-service",
            "task-management-workflow-configuration",
            "risk-assessment-framework-module",
        ]
        .join("\n");

        let violations = secret_violations(Path::new("README.md"), content.as_bytes());

        assert!(
            violations.is_empty(),
            "ordinary kebab-case prose must not look like a provider key: {violations:?}"
        );
    }

    #[test]
    fn secret_scan_allows_non_key_tokens_starting_with_sk() {
        let prefix = ["s", "k"].concat();
        let content = [
            [prefix.as_str(), "learn", "model-training-pipeline-v2"].join("-"),
            [prefix.as_str(), "icon", "set-large-collection"].join("-"),
            [prefix.as_str(), "session", "key-rotation-interval-seconds"].join("-"),
        ]
        .join("\n");

        let violations = secret_violations(Path::new("README.md"), content.as_bytes());

        assert!(
            violations.is_empty(),
            "non-key sk-prefixed tokens must not be reported: {violations:?}"
        );
    }

    #[test]
    fn secret_scan_treats_bare_jwks_filename_as_sensitive() {
        let violations = secret_violations(Path::new("jwks.json"), b"{}");

        assert_eq!(
            violations,
            vec![1],
            "a bare private JWKS filename must fail regardless of content"
        );
    }

    #[test]
    fn register_ignores_assignment_shaped_prose() {
        let content = r#"
schema = 1

[meta]
pinned_openshell = "v0.0.82"
pinned_spire = "1.15.2"
pinned_litellm = "1.93.0"
pinned_agent_sandbox = "v0.5.0"
pinned_mcp_gw = "v0.2.0"

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
        let content = format!(
            "schema = 1\n[meta]\npinned_openshell = \"v0.0.82\"\npinned_spire = \"1.15.2\"\npinned_litellm = \"1.93.0\"\npinned_agent_sandbox = \"v0.5.0\"\npinned_mcp_gw = \"v0.2.0\"\n{guarantees}"
        );

        let validation = validate_register_content(&content);

        assert_eq!(
            validation,
            Err("conformance register must not contain a hand-authored status".to_owned()),
            "a structural status key must remain forbidden"
        );
    }

    #[test]
    fn register_requires_complete_conformance_environment_provenance() {
        let guarantees = (1..=6)
            .map(|id| format!("[[guarantee]]\nid = \"G-{id}\"\n"))
            .collect::<String>();
        let content = format!("schema = 1\n[meta]\npinned_openshell = \"v0.0.90\"\n{guarantees}");

        let validation = validate_register_content(&content);

        assert_eq!(
            validation,
            Err("conformance register must pin SPIRE in one place".to_owned()),
            "a foundation claim without its SPIRE version has ambiguous provenance"
        );
    }

    #[test]
    fn register_status_is_derived_from_negative_test_outcomes() {
        assert_eq!(
            derive_status(false, &[TestOutcome::Passed], &[]),
            DerivedStatus::Provided,
            "green holds evidence must derive provided"
        );
        assert_eq!(
            derive_status(true, &[TestOutcome::Passed], &[TestOutcome::Passed]),
            DerivedStatus::Partial,
            "declared green gap evidence must cap the derived status at partial"
        );
        assert_eq!(
            derive_status(true, &[], &[TestOutcome::Passed]),
            DerivedStatus::NotYetProvided,
            "green gap-only evidence must derive not-yet-provided"
        );
        assert_eq!(
            derive_status(false, &[TestOutcome::Failed], &[]),
            DerivedStatus::Regressed,
            "a failed holds test must be a regression finding"
        );
        assert_eq!(
            derive_status(true, &[], &[TestOutcome::Failed]),
            DerivedStatus::GapMayHaveClosed,
            "a failed gap test must report possible upstream improvement"
        );
        assert_eq!(
            derive_status(false, &[], &[]),
            DerivedStatus::Unevidenced,
            "an entry with no tests must stay visibly unevidenced"
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
    fn migration_base_falls_back_to_local_then_remote_main() {
        assert_eq!(
            migration_base_candidates(None),
            vec!["main", "origin/main"],
            "local checks must work without an origin remote while detecting stale refs"
        );
    }

    #[test]
    fn migration_base_ignores_an_all_zero_event_sha() {
        let zero_sha = "0000000000000000000000000000000000000000";

        assert_eq!(
            migration_base_candidates(Some(zero_sha)),
            vec!["main", "origin/main"],
            "an all-zero push event SHA must fall back to real repository refs"
        );
    }

    #[test]
    fn migration_base_honors_an_explicit_commit() {
        let commit = "1234567890abcdef1234567890abcdef12345678";

        assert_eq!(
            migration_base_candidates(Some(commit)),
            vec![commit],
            "CI must compare against its explicit event commit"
        );
    }

    #[test]
    fn migration_base_prefers_local_main_when_remote_main_differs() {
        let candidates = vec!["main".to_owned(), "origin/main".to_owned()];
        let resolved = vec![
            ("main".to_owned(), "1111111".to_owned()),
            ("origin/main".to_owned(), "2222222".to_owned()),
        ];

        let selection = select_migration_base(&candidates, &resolved);

        assert_eq!(
            selection,
            Ok("main".to_owned()),
            "an advancing remote must not invalidate the branch's synced local base"
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
