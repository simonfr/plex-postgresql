# Interpose Maintenance Checklist

Use this checklist when adding or changing step/interpose helpers.

1. Keep `src/interpose/db_interpose_step.c` orchestration-only where possible.
2. Use `step_result_t` for helper return values (`ROW/DONE/ERROR/FALLBACK`).
3. If helper can fail at connection level, expose `conn_error_out` and set it explicitly.
4. Document lock ownership in header comments (entry lock, exit lock state).
5. Be explicit about state side effects (`write_executed`, `read_done`, cached stmt updates).
6. Centralize libpq cancel/drain behavior via `step_conn_cancel_and_drain`.
7. Prefer helper modules (`read_utils`, `write_utils`, `cached_read_utils`) over inline logic in `step.c`.
8. Add/keep fast compile coverage by running `make interpose-build-check`.
9. Keep logging taxonomy consistent (`READ`, `WRITE`, `CACHED_READ`, `CACHED_WRITE`, `LIFECYCLE`).
10. Update `docs/step-flow.md` when module boundaries or contracts change.
