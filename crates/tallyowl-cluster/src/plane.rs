//! The control plane: one way in, whether or not there is a quorum.
//!
//! A home installation has one controller and no quorum, and a cell has three
//! or five. Both accept the same operations and both hold the same rules, so
//! the difference is one branch in [`ControlPlane::apply`] rather than two code
//! paths that drift.
//!
//! **The rules do not live here.** [`crate::topology::Topology::apply`] holds
//! them, so a command that arrives through consensus and a command applied
//! directly are checked by the same code. A rule enforced only on the way in
//! would be a rule a replayed log could break.
//!
//! # What stops when a quorum is gone
//!
//! `docs/CELLS.md` section 10: "A cell-controller failure does not stop a
//! healthy tablet group. It prevents tablet placement changes until the
//! controller quorum returns." So a placement change refuses and says why, and
//! a read of the topology answers from what this node already holds.

use std::sync::{Arc, Mutex};

use tallyowl_obs::error::{ErrorCode, TallyOwlError};

use crate::controller::Controller;
use crate::directory::{Directory, DirectoryCache, DirectoryCommand};
use crate::groups::{GroupKey, GroupRegistry};
use crate::raft::machine::Outcome;
use crate::topology::{ControllerCommand, Generation, Topology};

/// Everything an operator or a gateway reaches.
pub struct ControlPlane {
    registry: Arc<GroupRegistry>,
    topology: Arc<Mutex<Topology>>,
    directory: Arc<Mutex<Directory>>,
    directory_cache: Arc<DirectoryCache>,
    controller: Controller,
    cell: String,
}

impl ControlPlane {
    pub fn new(
        registry: Arc<GroupRegistry>,
        topology: Arc<Mutex<Topology>>,
        controller: Controller,
    ) -> ControlPlane {
        let cell = controller.cell.clone();
        let directory = Directory::new();
        let directory_cache = DirectoryCache::new();
        // **An in-process directory is reachable.** `docs/CELLS.md` section 3
        // puts the global directory role inside the head for the home profile,
        // and a cache that started as unreachable would refuse every project
        // placement in an installation whose directory is a field on this
        // struct. The cache is for a directory that lives somewhere else, and
        // it is marked unreachable when a call to that one fails.
        directory_cache.refresh(directory.clone(), tallyowl_obs::time::now_ms());
        ControlPlane {
            registry,
            topology,
            directory: Arc::new(Mutex::new(directory)),
            directory_cache: Arc::new(directory_cache),
            controller,
            cell,
        }
    }

    pub fn controller(&self) -> &Controller {
        &self.controller
    }

    pub fn registry(&self) -> Arc<GroupRegistry> {
        Arc::clone(&self.registry)
    }

    pub fn topology(&self) -> Topology {
        self.topology.lock().expect("topology").clone()
    }

    pub fn shared_topology(&self) -> Arc<Mutex<Topology>> {
        Arc::clone(&self.topology)
    }

    pub fn directory(&self) -> Directory {
        self.directory.lock().expect("directory").clone()
    }

    pub fn directory_cache(&self) -> Arc<DirectoryCache> {
        Arc::clone(&self.directory_cache)
    }

    fn controller_group(&self) -> GroupKey {
        GroupKey::CellController(self.cell.clone())
    }

    /// Whether this cell has a controller quorum at all.
    ///
    /// A home installation does not, and that is a supported shape rather than
    /// a degradation: `docs/CELLS.md` section 3 says the home profile "does not
    /// require a controller quorum".
    pub fn has_quorum(&self) -> bool {
        self.registry.holds(&self.controller_group())
    }

    /// Apply one topology change.
    pub fn apply(&self, command: ControllerCommand) -> Result<Generation, TallyOwlError> {
        if !self.has_quorum() {
            // One controller and no quorum. The rules still hold, because they
            // are in the state machine and this calls the same one.
            let mut topology = self.topology.lock().expect("topology");
            return topology.apply(&command).map_err(topology_error);
        }
        let payload = crate::raft::encode(&command).map_err(TallyOwlError::internal)?;
        match self.registry.propose(&self.controller_group(), payload)? {
            Outcome::Applied { generation } | Outcome::Unchanged { generation } => Ok(generation),
            Outcome::Refused { reason } => {
                Err(TallyOwlError::new(ErrorCode::FailedPrecondition, reason))
            }
            other => Err(TallyOwlError::internal(format!(
                "The controller answered a topology change with {other:?}."
            ))),
        }
    }

    /// Apply one directory change.
    ///
    /// The four operations `docs/CELLS.md` section 9 names as stopping during a
    /// directory outage all arrive here, and all refuse together.
    pub fn apply_directory(&self, command: DirectoryCommand) -> Result<Generation, TallyOwlError> {
        self.directory_cache
            .may_change_placement()
            .map_err(|e| TallyOwlError::new(ErrorCode::Unavailable, e.to_string()))?;
        if !self.registry.holds(&GroupKey::GlobalDirectory) {
            let mut directory = self.directory.lock().expect("directory");
            let generation = directory
                .apply(&command)
                .map_err(|e| TallyOwlError::new(ErrorCode::FailedPrecondition, e.to_string()))?;
            self.directory_cache
                .refresh(directory.clone(), tallyowl_obs::time::now_ms());
            return Ok(generation);
        }
        let payload = crate::raft::encode(&command).map_err(TallyOwlError::internal)?;
        match self.registry.propose(&GroupKey::GlobalDirectory, payload)? {
            Outcome::Applied { generation } | Outcome::Unchanged { generation } => Ok(generation),
            Outcome::Refused { reason } => {
                Err(TallyOwlError::new(ErrorCode::FailedPrecondition, reason))
            }
            other => Err(TallyOwlError::internal(format!(
                "The directory answered a change with {other:?}."
            ))),
        }
    }

    /// Apply several commands as one intention.
    ///
    /// Unsafe recovery produces a list, and applying part of it would leave a
    /// working tablet with no record that anything was lost. This stops at the
    /// first refusal and reports it with what had already been applied, so an
    /// operator is never left guessing which half ran.
    pub fn apply_all(&self, commands: Vec<ControllerCommand>) -> Result<Generation, TallyOwlError> {
        let mut generation = self.topology().generation;
        for (index, command) in commands.iter().enumerate() {
            match self.apply(command.clone()) {
                Ok(next) => generation = next,
                Err(e) => {
                    return Err(TallyOwlError::new(
                        e.code,
                        format!(
                            "{} of {} changes were applied and then this one was refused: {}",
                            index,
                            commands.len(),
                            e.message
                        ),
                    ))
                }
            }
        }
        Ok(generation)
    }
}

fn topology_error(error: crate::topology::TopologyError) -> TallyOwlError {
    use crate::topology::TopologyError;
    match error {
        TopologyError::NotFound(message) => TallyOwlError::new(ErrorCode::NotFound, message),
        TopologyError::Refused(message) => {
            TallyOwlError::new(ErrorCode::FailedPrecondition, message)
        }
        TopologyError::AlreadyDone(message) => {
            TallyOwlError::new(ErrorCode::AlreadyExists, message)
        }
    }
}
