# Agent Guidelines

- Use Conventional Commits for commit messages and PR titles, e.g. `fix: install new-window hook on workspace create`.
- Follow the project-specific development and testing guidance in `CLAUDE.md`.
- When adding, renaming, or removing a workspace crate, update the GitHub Actions workflows—especially the release dispatch inputs and dependency-ordered crates.io publish list—in the same change.
