Contributing

Thanks for considering contributions. Keep things small and focused: a single issue or PR per problem or feature.

Getting started (developer)

- Install Rust (recommended rustup toolchain). Project targets rustc >= 1.94.
- Build locally: `cargo build --release`
- Run tests: `cargo test`
- Code style: run `cargo fmt` before opening a PR. Keep changes small and include tests when practical.

How to file issues

- Use the bug or feature templates provided in .github/ISSUE_TEMPLATE.
- For bugs provide: steps to reproduce, example image(s) or EXIF, exact command line, expected vs actual behaviour, and any backtrace or log output.

Pull requests

- Fork → branch from main (or the repository default) → make changes → run tests locally → open PR against the default branch.
- Provide a clear description and link to related issues. Include a short changelog entry if this affects users.

CI and releases

- The repository includes tests; keep them passing. Release binaries are attached to GitHub releases (maintainer action).

Code of conduct

- Be respectful. Keep discussions constructive. If you have concerns about community behaviour, open a private issue.
