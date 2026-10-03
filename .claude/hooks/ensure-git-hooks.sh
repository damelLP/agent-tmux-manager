#!/usr/bin/env bash
# SessionStart: make sure the git hooks are active for agent sessions.
# Humans enable them once with: git config core.hooksPath .githooks
cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0
if [ "$(git config --get core.hooksPath)" != ".githooks" ]; then
    git config core.hooksPath .githooks
    echo "Activated git hooks (core.hooksPath=.githooks)."
fi
exit 0
