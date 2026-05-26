//! Workspace detection.
//!
//! The detector is intentionally small and file-based. It feeds
//! session startup decisions such as which domain knowledge skills to
//! attach and which build system the workspace naturally uses.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildSystem {
    Make,
    Meson,
    Unknown,
}

impl BuildSystem {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Make => "make",
            Self::Meson => "meson",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceKind {
    LinuxKernel,
    Systemd,
    Unknown,
}

impl WorkspaceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LinuxKernel => "linux-kernel",
            Self::Systemd => "systemd",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceProfile {
    pub kind: WorkspaceKind,
    pub build_system: BuildSystem,
    pub knowledge_skills: Vec<&'static str>,
}

impl WorkspaceProfile {
    pub fn unknown() -> Self {
        Self {
            kind: WorkspaceKind::Unknown,
            build_system: BuildSystem::Unknown,
            knowledge_skills: Vec::new(),
        }
    }
}

pub fn detect_workspace(workspace: &Path) -> WorkspaceProfile {
    if is_linux_kernel_tree(workspace) {
        return WorkspaceProfile {
            kind: WorkspaceKind::LinuxKernel,
            build_system: BuildSystem::Make,
            knowledge_skills: vec!["kernel"],
        };
    }
    if is_systemd_tree(workspace) {
        return WorkspaceProfile {
            kind: WorkspaceKind::Systemd,
            build_system: BuildSystem::Meson,
            knowledge_skills: vec!["systemd"],
        };
    }
    WorkspaceProfile::unknown()
}

fn is_linux_kernel_tree(root: &Path) -> bool {
    root.join("Kconfig").is_file()
        && root.join("Kbuild").is_file()
        && root.join("Makefile").is_file()
        && root.join("include").join("linux").is_dir()
}

fn is_systemd_tree(root: &Path) -> bool {
    root.join("meson.build").is_file()
        && root.join("src").join("systemd").is_dir()
        && root.join("units").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(nonce: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("kres-workspace-{nonce}-"))
            .tempdir()
            .unwrap()
    }

    fn touch(path: &Path) {
        std::fs::write(path, "").unwrap();
    }

    #[test]
    fn detects_linux_kernel_workspace() {
        let dir = tmpdir("kernel");
        touch(&dir.path().join("Kconfig"));
        touch(&dir.path().join("Kbuild"));
        touch(&dir.path().join("Makefile"));
        std::fs::create_dir_all(dir.path().join("include/linux")).unwrap();

        let profile = detect_workspace(dir.path());

        assert_eq!(profile.kind, WorkspaceKind::LinuxKernel);
        assert_eq!(profile.build_system, BuildSystem::Make);
        assert_eq!(profile.knowledge_skills, vec!["kernel"]);
    }

    #[test]
    fn detects_systemd_workspace() {
        let dir = tmpdir("systemd");
        touch(&dir.path().join("meson.build"));
        std::fs::create_dir_all(dir.path().join("src/systemd")).unwrap();
        std::fs::create_dir_all(dir.path().join("units")).unwrap();

        let profile = detect_workspace(dir.path());

        assert_eq!(profile.kind, WorkspaceKind::Systemd);
        assert_eq!(profile.build_system, BuildSystem::Meson);
        assert_eq!(profile.knowledge_skills, vec!["systemd"]);
    }

    #[test]
    fn unknown_workspace_has_no_build_or_knowledge() {
        let dir = tmpdir("unknown");

        let profile = detect_workspace(dir.path());

        assert_eq!(profile.kind, WorkspaceKind::Unknown);
        assert_eq!(profile.build_system, BuildSystem::Unknown);
        assert!(profile.knowledge_skills.is_empty());
    }
}
