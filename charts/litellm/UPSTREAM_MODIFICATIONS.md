# Upstream modifications

Chart preparation removes the upstream `templates/tests` directory because
those optional Helm test hooks pull a mutable `busybox` image. The LiteLLM
workload templates and runtime configuration remain unchanged.
