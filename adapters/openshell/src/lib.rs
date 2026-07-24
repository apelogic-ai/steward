//! Thin OpenShell integration seam.

use sha2::{Digest, Sha256};

pub const IMPLEMENTED_PORTS: [&str; 0] = [];
const NAME_LENGTH: usize = 19;
const HASH_CHARACTERS: usize = NAME_LENGTH - 2;
const LOWER_BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

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
    let mut quotient = digest;
    let mut encoded = [b'0'; HASH_CHARACTERS];
    for digit in encoded.iter_mut().rev() {
        let mut remainder = 0_u16;
        for byte in &mut quotient {
            let value = (remainder << 8) | u16::from(*byte);
            *byte = (value / 36) as u8;
            remainder = value % 36;
        }
        *digit = LOWER_BASE36[usize::from(remainder)];
    }
    for byte in encoded {
        name.push(char::from(byte));
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
            workspace, "w-9086ou4eujpgku8z0",
            "workspace names must encode the full 17-character base36 budget"
        );
        assert_eq!(
            sandbox, "s-78i56shpq2adzg64z",
            "sandbox names must encode the full 17-character base36 budget"
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
