# Definition of Done

A task is complete only when:

- Scope and assumptions are documented.
- Acceptance criteria are automated where practical.
- Code respects dependency boundaries.
- No second fact source is introduced.
- State/error/enum changes update contracts and docs.
- Database changes include immutable migration and replay tests.
- Secrets and privacy impact are reviewed.
- Failure paths are tested.
- Format/lint/build/unit/contract tests pass.
- Exact verification commands and results are reported.
- No mandatory work is left behind as TODO.
- Current Sprint exit gate passes.

A task is not complete when only the happy path or UI mock is working.
