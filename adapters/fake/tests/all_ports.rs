use std::collections::BTreeSet;
use steward_adapter_fake::IMPLEMENTED_PORTS;
use steward_ports::PORTS;

#[test]
fn fake_adapter_implements_every_declared_port() {
    let declared = PORTS.iter().map(|port| port.name).collect::<BTreeSet<_>>();
    let implemented = IMPLEMENTED_PORTS.into_iter().collect::<BTreeSet<_>>();

    assert_eq!(
        implemented, declared,
        "fake adapter port declarations must exactly match the port registry"
    );
}
