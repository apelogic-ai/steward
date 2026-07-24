//! Thin OpenShell integration seam.

use sha2::{Digest, Sha256};

pub const IMPLEMENTED_PORTS: [&str; 0] = [];
const NAME_LENGTH: usize = 19;
const HASH_CHARACTERS: usize = NAME_LENGTH - 2;
const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NameKind {
    Workspace,
    Sandbox,
}

pub fn stable_name(kind: NameKind, identity: &[u8]) -> String {
    let prefix = match kind {
        NameKind::Workspace => "w-",
        NameKind::Sandbox => "s-",
    };
    let digest = Sha256::digest(identity);
    let mut name = String::with_capacity(NAME_LENGTH);
    name.push_str(prefix);
    for nibble_index in 0..HASH_CHARACTERS {
        let byte = digest[nibble_index / 2];
        let nibble = if nibble_index % 2 == 0 {
            byte >> 4
        } else {
            byte & 0x0f
        };
        name.push(char::from(LOWER_HEX[usize::from(nibble)]));
    }
    name
}

#[cfg(test)]
mod tests {
    use super::{NameKind, stable_name};

    #[test]
    fn stable_names_fit_the_immutable_openshell_cap() {
        let workspace = stable_name(NameKind::Workspace, b"team-a");
        let sandbox = stable_name(NameKind::Sandbox, b"runtime-uid-1");

        assert_eq!(
            workspace, "w-96c2886c51d1dfb49",
            "workspace names must use the stable SHA-256 derivation"
        );
        assert_eq!(
            sandbox, "s-20d730b3c5fe542e0",
            "sandbox names must use a distinct stable SHA-256 domain"
        );
        for name in [&workspace, &sandbox] {
            assert_eq!(name.len(), 19, "OpenShell names must fit its 19-char cap");
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
                "OpenShell names must remain DNS-safe: {name}"
            );
        }
    }
}
