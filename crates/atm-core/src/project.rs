//! Project and worktree resolution utilities.
//!
//! Resolves git project roots and worktree information from working directories.

use std::path::Path;

/// Resolves the git project root from a working directory.
/// Walks up the directory tree looking for `.git`.
///
/// For regular repos, `.git` is a directory. For worktrees, `.git` is a file
/// containing a `gitdir:` pointer — in that case, we follow the pointer back
/// to the main repository root.
pub fn resolve_project_root(working_dir: &str) -> Option<String> {
    let mut path = Path::new(working_dir);
    loop {
        let git_path = path.join(".git");
        if git_path.is_dir() {
            // Standard repo — .git is a directory
            return Some(path.to_string_lossy().to_string());
        }
        if git_path.is_file() {
            // Worktree — .git is a file containing "gitdir: /path/to/main/.git/worktrees/<name>"
            if let Ok(content) = std::fs::read_to_string(&git_path) {
                if let Some(gitdir_value) = content.strip_prefix("gitdir:") {
                    let gitdir = gitdir_value.trim();
                    // Walk ancestors of the gitdir path to find the .git directory
                    if let Some(main_git) = Path::new(gitdir)
                        .ancestors()
                        .find(|p| p.file_name().map_or(false, |n| n == ".git"))
                    {
                        if let Some(parent) = main_git.parent() {
                            return Some(parent.to_string_lossy().to_string());
                        }
                    }
                }
            }
            // Fallback: treat this directory as the root
            return Some(path.to_string_lossy().to_string());
        }
        path = path.parent()?;
    }
}

/// Resolves worktree information from a working directory.
/// Returns `(worktree_path, branch_name)`.
///
/// Returns `(None, None)` if the directory is not inside a git repository.
pub fn resolve_worktree_info(working_dir: &str) -> (Option<String>, Option<String>) {
    let path = Path::new(working_dir);
    let git_entry = path.join(".git");
    // Only resolve if we're at a git root (either .git dir or .git worktree file)
    if !git_entry.is_dir() && !git_entry.is_file() {
        return (None, None);
    }
    let branch = resolve_branch_name(path);
    (Some(working_dir.to_string()), branch)
}

/// Reads the HEAD reference to determine the current branch name.
/// Returns `None` if HEAD cannot be read.
fn resolve_branch_name(repo_path: &Path) -> Option<String> {
    // For a normal repo, HEAD is at .git/HEAD
    // For a worktree, .git is a file; HEAD is inside the gitdir
    let git_path = repo_path.join(".git");

    let head_path = if git_path.is_file() {
        // Worktree: read gitdir pointer, then find HEAD there
        let content = std::fs::read_to_string(&git_path).ok()?;
        let gitdir = content.strip_prefix("gitdir:")?.trim();
        Path::new(gitdir).join("HEAD")
    } else {
        git_path.join("HEAD")
    };

    let content = std::fs::read_to_string(&head_path).ok()?;
    let trimmed = content.trim();
    if let Some(ref_name) = trimmed.strip_prefix("ref: refs/heads/") {
        Some(ref_name.to_string())
    } else {
        // Detached HEAD — return short SHA
        Some(trimmed.chars().take(8).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_resolve_project_root_standard_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("myrepo");
        fs::create_dir_all(repo_path.join(".git")).unwrap();
        let src = repo_path.join("src");
        fs::create_dir_all(&src).unwrap();

        let result = resolve_project_root(src.to_str().unwrap());
        assert_eq!(result, Some(repo_path.to_string_lossy().to_string()));
    }

    #[test]
    fn test_resolve_project_root_no_git() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_project_root(dir.path().to_str().unwrap());
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_project_root_at_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();

        let result = resolve_project_root(dir.path().to_str().unwrap());
        assert_eq!(result, Some(dir.path().to_string_lossy().to_string()));
    }

    #[test]
    fn test_resolve_branch_name_attached() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(
            dir.path().join(".git/HEAD"),
            "ref: refs/heads/main\n",
        )
        .unwrap();

        let branch = resolve_branch_name(dir.path());
        assert_eq!(branch, Some("main".to_string()));
    }

    #[test]
    fn test_resolve_branch_name_detached() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(
            dir.path().join(".git/HEAD"),
            "abc123def456789\n",
        )
        .unwrap();

        let branch = resolve_branch_name(dir.path());
        assert_eq!(branch, Some("abc123de".to_string()));
    }

    #[test]
    fn test_resolve_worktree_info_with_branch() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(
            dir.path().join(".git/HEAD"),
            "ref: refs/heads/feature-x\n",
        )
        .unwrap();

        let (wt_path, branch) = resolve_worktree_info(dir.path().to_str().unwrap());
        assert_eq!(wt_path, Some(dir.path().to_str().unwrap().to_string()));
        assert_eq!(branch, Some("feature-x".to_string()));
    }

    #[test]
    fn test_resolve_worktree_info_no_git() {
        let dir = tempfile::tempdir().unwrap();
        let (wt_path, branch) = resolve_worktree_info(dir.path().to_str().unwrap());
        assert!(wt_path.is_none(), "worktree_path should be None outside git repo");
        assert!(branch.is_none(), "branch should be None outside git repo");
    }

    #[test]
    fn test_resolve_project_root_worktree() {
        let dir = tempfile::tempdir().unwrap();
        // Set up main repo
        let main_repo = dir.path().join("main-repo");
        let git_dir = main_repo.join(".git");
        fs::create_dir_all(git_dir.join("worktrees/feature")).unwrap();

        // Set up worktree with .git file pointing to main
        let worktree = dir.path().join("worktree-feature");
        fs::create_dir_all(&worktree).unwrap();
        let gitdir_path = git_dir.join("worktrees/feature");
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}", gitdir_path.to_string_lossy()),
        )
        .unwrap();

        let result = resolve_project_root(worktree.to_str().unwrap());
        assert_eq!(result, Some(main_repo.to_string_lossy().to_string()));
    }
}
