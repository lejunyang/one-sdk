# Development Workflow

- After completing and validating each independent task or feature, create a dedicated Git commit before starting the next task.
- Do not batch multiple already-completed features into one commit when they can be separated safely.
- Keep each commit focused on one behavior change and include its tests and directly related documentation.
- For every user-facing capability change, review `README.md`, `site/guide/`, and `site/en/guide/`; update every affected document in the same commit so the root README and both site languages stay aligned with the implementation.
- Every commit message must end with this trailer exactly once:

  `Co-authored-by: TRAE CLI <noreply@bytedance.com>`

- Run the narrowest relevant tests before each commit. Run the full workspace validation before declaring a multi-commit initiative complete.
- Tests and smoke checks must use temporary `HOME`, `OSDK_*`, `CARGO_HOME`, `RUSTUP_HOME`, and build directories where applicable. Do not modify or rely on the user's real SDK-manager state.
