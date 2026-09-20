use std::path::{Component, Path, PathBuf};

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn absolute_lexical(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(lexical_normalize(path));
    }
    std::env::current_dir()
        .map(|cwd| lexical_normalize(&cwd.join(path)))
        .map_err(|error| format!("Failed to resolve current directory: {error}"))
}

fn path_escape_error(path: &Path) -> String {
    format!("Path escapes workspace root: {}", path.display())
}

pub(crate) fn infer_project_root(
    workspace_root: &Path,
    candidate: &Path,
) -> Result<PathBuf, String> {
    let workspace_lexical = absolute_lexical(workspace_root)?;
    let workspace = workspace_root
        .canonicalize()
        .map_err(|error| format!("Failed to resolve workspace root: {error}"))?;
    if !workspace.is_dir() {
        return Err(format!(
            "Workspace root is not a directory: {}",
            workspace.display()
        ));
    }

    let candidate_lexical = if candidate.is_absolute() {
        lexical_normalize(candidate)
    } else {
        lexical_normalize(&workspace_lexical.join(candidate))
    };
    let candidate_in_canonical_namespace =
        if let Ok(relative) = candidate_lexical.strip_prefix(&workspace_lexical) {
            workspace.join(relative)
        } else {
            candidate_lexical.clone()
        };
    if !candidate_in_canonical_namespace.starts_with(&workspace) {
        return Err(path_escape_error(candidate));
    }

    let mut nearest_existing = candidate_in_canonical_namespace.clone();
    while !nearest_existing.exists() {
        if nearest_existing == workspace || !nearest_existing.pop() {
            break;
        }
    }
    let nearest_existing = nearest_existing
        .canonicalize()
        .map_err(|error| format!("Failed to resolve project path: {error}"))?;
    if !nearest_existing.starts_with(&workspace) {
        return Err(path_escape_error(candidate));
    }

    if candidate_in_canonical_namespace == workspace {
        return Ok(workspace);
    }

    let mut current = if nearest_existing.is_dir() {
        nearest_existing.clone()
    } else {
        nearest_existing
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| workspace.clone())
    };
    loop {
        if current.join(".git").exists() {
            return Ok(current);
        }
        if current == workspace || !current.pop() {
            break;
        }
    }

    let fallback_source = if nearest_existing != workspace {
        nearest_existing.as_path()
    } else {
        candidate_in_canonical_namespace.as_path()
    };
    if let Ok(relative) = fallback_source.strip_prefix(&workspace)
        && let Some(first) = relative.components().next()
        && matches!(first, Component::Normal(_))
    {
        let top_level = workspace.join(first.as_os_str());
        if let Ok(canonical_top_level) = top_level.canonicalize()
            && canonical_top_level.is_dir()
            && canonical_top_level.starts_with(&workspace)
        {
            return Ok(canonical_top_level);
        }
    }

    Ok(workspace)
}

pub(crate) fn valid_active_project(
    workspace_root: &Path,
    active_project: Option<&Path>,
) -> Option<PathBuf> {
    let active_project = active_project?;
    let workspace = workspace_root.canonicalize().ok()?;
    let project = active_project.canonicalize().ok()?;
    (project.is_dir() && project.starts_with(&workspace)).then_some(project)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn workspace(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "catdesk-project-scope-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create test workspace");
        root
    }

    fn canonical(path: &Path) -> PathBuf {
        path.canonicalize().expect("canonicalize test path")
    }

    #[test]
    fn nearest_git_root_wins_without_scanning_siblings() {
        let root = workspace("nearest-git");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        fs::create_dir_all(repo_a.join(".git")).expect("create repo-a git marker");
        fs::create_dir_all(repo_a.join("src")).expect("create repo-a src");
        fs::create_dir_all(repo_b.join(".git")).expect("create repo-b git marker");
        fs::write(repo_a.join("src/lib.rs"), "pub fn a() {}\n").expect("write candidate");

        let project =
            infer_project_root(&root, &repo_a.join("src/lib.rs")).expect("infer project root");

        assert_eq!(project, canonical(&repo_a));
        assert_ne!(project, canonical(&repo_b));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn non_git_path_falls_back_to_top_level_workspace_child() {
        let root = workspace("top-level-fallback");
        let project = root.join("plain-project");
        fs::create_dir_all(project.join("sub")).expect("create project tree");
        fs::write(project.join("sub/file.txt"), "hello\n").expect("write candidate");

        let inferred = infer_project_root(&root, &project.join("sub/file.txt"))
            .expect("infer fallback project");

        assert_eq!(inferred, canonical(&project));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_root_can_be_an_explicit_project_scope() {
        let root = workspace("workspace-root");

        let inferred = infer_project_root(&root, &root).expect("infer workspace root");

        assert_eq!(inferred, canonical(&root));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_candidate_cannot_escape_workspace() {
        let root = workspace("escape");
        let outside = root
            .parent()
            .expect("workspace parent")
            .join(format!("outside-{}", std::process::id()));
        fs::create_dir_all(&outside).expect("create outside path");

        let error = infer_project_root(&root, &outside).expect_err("outside path must fail");

        assert!(error.contains("Path escapes workspace root"), "{error}");
        let _ = fs::remove_dir_all(outside);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stale_active_project_is_rejected() {
        let root = workspace("stale");
        let project = root.join("repo");
        fs::create_dir_all(&project).expect("create project");
        let selected = canonical(&project);
        fs::remove_dir_all(&project).expect("remove project");

        assert_eq!(valid_active_project(&root, Some(&selected)), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn nonexistent_leaf_uses_nearest_existing_ancestor() {
        let root = workspace("missing-leaf");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git")).expect("create git marker");
        let candidate = repo.join("new/deep/file.txt");
        assert!(!candidate.exists());

        let inferred = infer_project_root(&root, &candidate).expect("infer missing leaf project");

        assert_eq!(inferred, canonical(&repo));
        let _ = fs::remove_dir_all(root);
    }
}
