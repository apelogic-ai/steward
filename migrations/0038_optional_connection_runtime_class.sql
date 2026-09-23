ALTER TABLE connection_operations
    DROP CONSTRAINT connection_operations_runtime_class_check;

ALTER TABLE connection_operations
    ADD CONSTRAINT connection_operations_runtime_class_check
    CHECK (runtime_class = '' OR btrim(runtime_class) <> '');

COMMENT ON COLUMN connection_operations.runtime_class IS
    'Exact OpenShell RuntimeClass binding; empty selects the Kubernetes cluster default.';
