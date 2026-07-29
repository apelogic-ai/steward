DROP INDEX inference_exhaustions_by_runtime_spec;

CREATE INDEX inference_exhaustions_by_runtime
ON inference_exhaustions (
    runtime_uid,
    at DESC,
    id DESC
);
