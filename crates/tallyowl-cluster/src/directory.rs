//! The global project-to-cell directory.
//!
//! `docs/STORAGE.md` section 7: "The directory maps projects to regional cells.
//! It also stores cell identity and policy. The global directory does not store
//! telemetry. It does not manage tablets."
//!
//! # Why it is separate, and small
//!
//! Putting tablet placement here is the mistake this design exists to avoid. A
//! 10,000-node installation changes tablet placement constantly and changes
//! project placement almost never, so a directory that held both would be a
//! global consensus group on the write path. It holds one row for each project
//! instead, and a cell never asks it during a write.
//!
//! # What an outage costs, and what it does not
//!
//! `docs/CELLS.md` section 9 states it exactly: an existing cell keeps working
//! from its current assignments, and only new placement, project movement,
//! global policy, and new cell registration stop. [`DirectoryCache`] is what
//! makes that true in code rather than in prose — a cell reads its assignments
//! from a cached copy and never from a live call, so an unreachable directory
//! is a refusal on the operations above and silence everywhere else.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::topology::{CellName, Generation};

/// A project ID, as the 16 raw bytes it travels as.
pub type ProjectId = [u8; 16];

/// Where one project lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub project_id: ProjectId,
    /// A project can be in more than one cell. The first is where a write goes;
    /// the rest hold read or export replicas.
    pub cells: Vec<CellName>,
    /// Set while the project is moving. `docs/CELLS.md` section 11: the source
    /// stays readable until old requests stop.
    pub moving_to: Option<CellName>,
    pub policy_generation: Generation,
}

/// Every change the directory quorum can make. Nothing else moves its state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectoryCommand {
    RegisterCell {
        cell: CellName,
        region: String,
    },
    AssignProject {
        project_id: ProjectId,
        cells: Vec<CellName>,
    },
    StartProjectMove {
        project_id: ProjectId,
        onto_cell: CellName,
    },
    FinishProjectMove {
        project_id: ProjectId,
    },
    SetGlobalPolicyGeneration {
        generation: Generation,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryError {
    NotFound(String),
    Refused(String),
}

impl std::fmt::Display for DirectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DirectoryError::NotFound(m) | DirectoryError::Refused(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for DirectoryError {}

/// The directory's whole state. This is the state machine of the global
/// directory quorum, and it is deliberately this short.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directory {
    pub generation: Generation,
    pub policy_generation: Generation,
    cells: BTreeMap<CellName, String>,
    placements: BTreeMap<ProjectId, Placement>,
}

impl Directory {
    pub fn new() -> Directory {
        Directory::default()
    }

    pub fn cells(&self) -> impl Iterator<Item = (&CellName, &String)> {
        self.cells.iter()
    }

    pub fn placement(&self, project_id: &ProjectId) -> Option<&Placement> {
        self.placements.get(project_id)
    }

    pub fn placements(&self) -> impl Iterator<Item = &Placement> {
        self.placements.values()
    }

    /// Which cell accepts a write for this project.
    ///
    /// A project that is moving still writes to its source cell. The target
    /// becomes the writer only when the move finishes, which is what keeps the
    /// move from having two writers at once.
    pub fn write_cell(&self, project_id: &ProjectId) -> Option<&CellName> {
        self.placements.get(project_id)?.cells.first()
    }

    pub fn apply(&mut self, command: &DirectoryCommand) -> Result<Generation, DirectoryError> {
        let changed = match command {
            DirectoryCommand::RegisterCell { cell, region } => {
                if self.cells.get(cell) == Some(region) {
                    false
                } else {
                    self.cells.insert(cell.clone(), region.clone());
                    true
                }
            }

            DirectoryCommand::AssignProject { project_id, cells } => {
                if cells.is_empty() {
                    return Err(DirectoryError::Refused(
                        "A project must be assigned to at least one cell.".into(),
                    ));
                }
                if let Some(unknown) = cells.iter().find(|c| !self.cells.contains_key(*c)) {
                    return Err(DirectoryError::NotFound(format!(
                        "No cell is named `{unknown}`. Register the cell before assigning a project to it."
                    )));
                }
                let existing = self.placements.get(project_id);
                if existing.map(|p| &p.cells) == Some(cells) {
                    false
                } else {
                    let policy_generation = self.policy_generation;
                    self.placements.insert(
                        *project_id,
                        Placement {
                            project_id: *project_id,
                            cells: cells.clone(),
                            moving_to: existing.and_then(|p| p.moving_to.clone()),
                            policy_generation,
                        },
                    );
                    true
                }
            }

            DirectoryCommand::StartProjectMove {
                project_id,
                onto_cell,
            } => {
                if !self.cells.contains_key(onto_cell) {
                    return Err(DirectoryError::NotFound(format!(
                        "No cell is named `{onto_cell}`."
                    )));
                }
                let placement = self.placements.get_mut(project_id).ok_or_else(|| {
                    DirectoryError::NotFound(
                        "That project is not assigned to a cell yet.".to_string(),
                    )
                })?;
                if placement.moving_to.as_ref() == Some(onto_cell) {
                    false
                } else {
                    placement.moving_to = Some(onto_cell.clone());
                    true
                }
            }

            DirectoryCommand::FinishProjectMove { project_id } => {
                let placement = self.placements.get_mut(project_id).ok_or_else(|| {
                    DirectoryError::NotFound(
                        "That project is not assigned to a cell yet.".to_string(),
                    )
                })?;
                match placement.moving_to.take() {
                    None => false,
                    Some(target) => {
                        placement.cells.retain(|c| *c != target);
                        placement.cells.insert(0, target);
                        true
                    }
                }
            }

            DirectoryCommand::SetGlobalPolicyGeneration { generation } => {
                if self.policy_generation == *generation {
                    false
                } else {
                    self.policy_generation = *generation;
                    true
                }
            }
        };
        if changed {
            self.generation += 1;
        }
        Ok(self.generation)
    }
}

/// A cell's copy of the directory answers it needs.
///
/// The cell reads this and never a live directory call, so a directory outage
/// stops nothing that this copy can answer. `docs/CELLS.md` section 9.
#[derive(Debug, Default)]
pub struct DirectoryCache {
    inner: std::sync::Mutex<CacheInner>,
}

#[derive(Debug, Default)]
struct CacheInner {
    directory: Directory,
    /// When the cell last heard from the directory. An operator sees this and
    /// knows how old the assignments are.
    refreshed_at: i64,
    reachable: bool,
}

impl DirectoryCache {
    pub fn new() -> DirectoryCache {
        DirectoryCache::default()
    }

    /// Replace the cached copy. Called when a directory answer arrives.
    pub fn refresh(&self, directory: Directory, at: i64) {
        let mut inner = self.inner.lock().expect("directory cache");
        inner.directory = directory;
        inner.refreshed_at = at;
        inner.reachable = true;
    }

    /// Record that the directory did not answer. The cached copy stays.
    pub fn unreachable(&self) {
        self.inner.lock().expect("directory cache").reachable = false;
    }

    pub fn reachable(&self) -> bool {
        self.inner.lock().expect("directory cache").reachable
    }

    pub fn refreshed_at(&self) -> i64 {
        self.inner.lock().expect("directory cache").refreshed_at
    }

    /// The cell a project writes to, from the cached copy.
    pub fn write_cell(&self, project_id: &ProjectId) -> Option<CellName> {
        self.inner
            .lock()
            .expect("directory cache")
            .directory
            .write_cell(project_id)
            .cloned()
    }

    pub fn snapshot(&self) -> Directory {
        self.inner
            .lock()
            .expect("directory cache")
            .directory
            .clone()
    }

    /// Whether an operation that needs a live directory may proceed.
    ///
    /// The four in `docs/CELLS.md` section 9 are new project placement, project
    /// movement, global policy changes, and new cell registration. Everything
    /// else reads the cache and does not ask.
    pub fn may_change_placement(&self) -> Result<(), DirectoryError> {
        if self.reachable() {
            return Ok(());
        }
        Err(DirectoryError::Refused(
            "The global directory is not answering, so a project cannot be placed or moved right now. Data operations in this cell are not affected and continue with the assignments the cell already holds.".into(),
        ))
    }
}
