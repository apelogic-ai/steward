use steward_adapter_fake::IMPLEMENTED_PORTS;
use steward_ports::PORTS;

#[test]
fn fake_adapter_implements_every_declared_port() {
    let missing = PORTS
        .iter()
        .map(|port| port.name)
        .filter(|name| !IMPLEMENTED_PORTS.contains(name))
        .collect::<Vec<_>>();

    assert!(
        missing.is_empty(),
        "fake adapter is missing declared ports: {missing:?}"
    );
}
