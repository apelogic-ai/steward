-- Applying the binding-aware release is an offline boundary for legacy versioned Tasks.
-- Hold writers while proving every pre-binding versioned Task has completed finalization.
LOCK TABLE task_submissions IN SHARE ROW EXCLUSIVE MODE;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM task_submissions
        WHERE workflow_name IS NOT NULL
          AND execution_binding IS NULL
          AND NOT finalized
    ) THEN
        RAISE EXCEPTION
            'execution binding rollout requires every legacy versioned Task to be finalized';
    END IF;
END
$$;

-- NOT VALID preserves finalized historical rows while enforcing the rule for every new write.
-- This makes an old apiserver fail closed after migration instead of creating work that a
-- binding-aware controller cannot safely interpret.
ALTER TABLE task_submissions
    ADD CONSTRAINT task_submissions_new_versioned_binding_required
        CHECK (workflow_name IS NULL OR execution_binding IS NOT NULL)
        NOT VALID;

COMMENT ON CONSTRAINT task_submissions_new_versioned_binding_required ON task_submissions IS
    'Staged-rollout fence: new versioned Task writes require an immutable execution binding; finalized pre-binding history remains readable.';
