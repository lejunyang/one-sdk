# Development Workflow

- After completing and validating each independent task or feature, create a dedicated Git commit before starting the next task.
- Do not batch multiple already-completed features into one commit when they can be separated safely.
- Keep each commit focused on one behavior change and include its tests and directly related documentation.
- For every user-facing capability change, review `README.md`, `README.zh-CN.md`, `site/guide/`, `site/en/guide/`, and `site/.vitepress/config.mts`; update every affected document in the same commit so both READMEs, both site languages, and navigation stay aligned with the implementation.
- Keep `README.md` and `README.zh-CN.md` as scenario-oriented entry points for product features and usage. Do not put internal architecture, implementation algorithms, or design rationale in either README; put that material in the matching VitePress implementation section instead.
- Keep every VitePress user-guide and implementation page paired in Chinese and English. When adding, removing, or renaming a page, update both locale sidebars and run the VitePress production build so dead links fail validation.

- Run the narrowest relevant tests before each commit. Run the full workspace validation before declaring a multi-commit initiative complete.
- Tests and smoke checks must use temporary `HOME`, `OSDK_*`, `CARGO_HOME`, `RUSTUP_HOME`, and build directories where applicable. Do not modify or rely on the user's real SDK-manager state.
