//! The executable bundle's single membership and publication-order authority.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleMember {
    Daemon = 0,
    Tui = 1,
    Cli = 2,
    WaylandPortal = 3,
}

/// Publish dependencies before the CLI that dispatches to them. Recovery uses
/// this same order and retains the marker until every member is coherent.
pub const BUNDLE_MEMBERS: [BundleMember; 3] =
    [BundleMember::Daemon, BundleMember::Tui, BundleMember::Cli];

/// Optional companion is published before the control executable as well.
/// Network macOS archives retain the three required members above.
pub const ALL_BUNDLE_MEMBERS: [BundleMember; 4] = [
    BundleMember::Daemon,
    BundleMember::Tui,
    BundleMember::WaylandPortal,
    BundleMember::Cli,
];

impl BundleMember {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Daemon => "haiderd",
            Self::Tui => "haider-tui",
            Self::Cli => "haider",
            Self::WaylandPortal => "haider-wayland-portal",
        }
    }

    pub const fn file_name(self) -> &'static str {
        if cfg!(windows) {
            match self {
                Self::Daemon => "haiderd.exe",
                Self::Tui => "haider-tui.exe",
                Self::Cli => "haider.exe",
                Self::WaylandPortal => "haider-wayland-portal",
            }
        } else {
            self.name()
        }
    }

    pub fn backup_prefix(self) -> String {
        format!(".{}-old-", self.name())
    }
}
