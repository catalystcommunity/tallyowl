//! Saved analyses, and the dashboards made of them.
//!
//! `docs/QUERY.md` section 20: a saved query stores the request, a name, and the
//! algebra version it was written against. A dashboard is an ordered list of
//! saved analyses with a layout, and nothing more: the panels are the saved
//! analyses, so a dashboard cannot hold a query that a person cannot also run
//! on its own.
//!
//! # Why a saved analysis stores its algebra version
//!
//! The version says what the request meant when somebody wrote it. A later
//! release that changes an operator can then answer the old request the old
//! way, or refuse it by name, rather than answering a different question under
//! the same title. `docs/QUERY.md` section 16 makes the version part of every
//! request for this reason, and a saved one keeps it.
//!
//! A saved analysis written for a **newer** version than this build speaks is
//! refused with both numbers, rather than run with the parts this build
//! understands. Half a query is a different query.
//!
//! # Why the panels hold identifiers rather than queries
//!
//! A dashboard that embedded its queries would let two panels drift from the
//! saved analysis they were made from, and a person editing the analysis would
//! find that the dashboard did not change. One definition, referenced.

use std::collections::BTreeMap;

use tallyowl_obs::error::{ErrorCode, TallyOwlError};

/// One saved analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    pub analysis_id: String,
    pub project_id: [u8; 16],
    pub name: String,
    /// What form of query this is: `node`, `funnel`, `retention`, `path`,
    /// `trace`, or `timeline`. It is stored so that a list can be grouped and
    /// so that a caller can refuse a form this build does not answer without
    /// decoding the request.
    pub form: String,
    /// The encoded `QueryRequest`, exactly as it would be sent.
    pub request: Vec<u8>,
    pub algebra_version: u64,
    pub created_at: i64,
    pub updated_at: i64,
    /// Who last wrote it. An analysis somebody relies on should say whose it
    /// is, and a change to a shared one should be attributable.
    pub updated_by: String,
}

/// One dashboard: an ordered list of panels and how they sit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dashboard {
    pub dashboard_id: String,
    pub project_id: [u8; 16],
    pub name: String,
    pub panels: Vec<Panel>,
    pub updated_at: i64,
    pub updated_by: String,
}

/// One panel of a dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Panel {
    pub analysis_id: String,
    /// What a person reads above the panel. Absent takes the analysis's name,
    /// so a panel does not have to repeat it and cannot silently disagree.
    pub title: Option<String>,
    /// Where it sits, in a twelve-column grid.
    pub column: u32,
    pub row: u32,
    pub width: u32,
    pub height: u32,
}

/// How wide the grid is. A width beyond it is refused rather than wrapped,
/// because a panel that wrapped would move every panel after it.
pub const COLUMNS: u32 = 12;

/// Saved analyses and dashboards, by project.
#[derive(Debug, Clone, Default)]
pub struct Saved {
    analyses: BTreeMap<([u8; 16], String), Analysis>,
    dashboards: BTreeMap<([u8; 16], String), Dashboard>,
}

impl Saved {
    pub fn new() -> Saved {
        Saved::default()
    }

    /// Store one analysis, or replace it.
    pub fn put_analysis(
        &mut self,
        analysis: Analysis,
        speaks_version: u64,
    ) -> Result<(), TallyOwlError> {
        if analysis.name.trim().is_empty() {
            return Err(TallyOwlError::invalid_argument(
                "A saved analysis needs a name. It is what somebody picks it out by.",
            ));
        }
        if analysis.request.is_empty() {
            return Err(TallyOwlError::invalid_argument(
                "This saved analysis carries no query. Send the query it saves.",
            ));
        }
        if analysis.algebra_version > speaks_version {
            return Err(TallyOwlError::new(
                ErrorCode::SchemaUnsupported,
                format!(
                    "This analysis was written for query version {} and this installation speaks version {speaks_version}. It is not saved, because saving it would put a query here that nothing here can answer.",
                    analysis.algebra_version
                ),
            ));
        }
        self.analyses.insert(
            (analysis.project_id, analysis.analysis_id.clone()),
            analysis,
        );
        Ok(())
    }

    pub fn analysis(&self, project_id: [u8; 16], analysis_id: &str) -> Option<&Analysis> {
        self.analyses.get(&(project_id, analysis_id.to_string()))
    }

    pub fn analyses(&self, project_id: [u8; 16]) -> Vec<&Analysis> {
        self.analyses
            .iter()
            .filter(|((project, _), _)| *project == project_id)
            .map(|(_, analysis)| analysis)
            .collect()
    }

    /// Remove one dashboard.
    ///
    /// The analyses it showed stay. An analysis is a question somebody saved
    /// and a dashboard is one arrangement of several, so removing the
    /// arrangement must not remove the questions: another dashboard may show
    /// the same analysis, and a person may run it on its own.
    pub fn remove_dashboard(
        &mut self,
        project_id: [u8; 16],
        dashboard_id: &str,
    ) -> Result<(), TallyOwlError> {
        if self
            .dashboards
            .remove(&(project_id, dashboard_id.to_string()))
            .is_none()
        {
            return Err(TallyOwlError::new(
                ErrorCode::NotFound,
                format!("There is no dashboard `{dashboard_id}` in this project."),
            ));
        }
        Ok(())
    }

    /// Remove one analysis.
    ///
    /// **A dashboard that shows it stops it.** Removing an analysis a panel
    /// points at would leave a dashboard with a hole and no explanation, so the
    /// refusal names the dashboards rather than leaving somebody to find them.
    pub fn remove_analysis(
        &mut self,
        project_id: [u8; 16],
        analysis_id: &str,
    ) -> Result<(), TallyOwlError> {
        let showing: Vec<&str> = self
            .dashboards
            .values()
            .filter(|dashboard| dashboard.project_id == project_id)
            .filter(|dashboard| {
                dashboard
                    .panels
                    .iter()
                    .any(|panel| panel.analysis_id == analysis_id)
            })
            .map(|dashboard| dashboard.name.as_str())
            .collect();
        if !showing.is_empty() {
            return Err(TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!(
                    "`{analysis_id}` is on {}: {}. Take it off those first, or they would show a panel with nothing behind it.",
                    if showing.len() == 1 {
                        "one dashboard"
                    } else {
                        "some dashboards"
                    },
                    showing.join(", ")
                ),
            ));
        }
        self.analyses
            .remove(&(project_id, analysis_id.to_string()))
            .map(|_| ())
            .ok_or_else(|| {
                TallyOwlError::new(
                    ErrorCode::NotFound,
                    format!("No saved analysis is named `{analysis_id}`."),
                )
            })
    }

    /// Store one dashboard, or replace it.
    pub fn put_dashboard(&mut self, dashboard: Dashboard) -> Result<(), TallyOwlError> {
        if dashboard.name.trim().is_empty() {
            return Err(TallyOwlError::invalid_argument("A dashboard needs a name."));
        }
        for panel in &dashboard.panels {
            // Every panel names an analysis that exists **in this project**. A
            // panel pointing at another project's analysis would show one
            // project's data under another's name, which is worse than an
            // empty panel.
            if self
                .analysis(dashboard.project_id, &panel.analysis_id)
                .is_none()
            {
                return Err(TallyOwlError::new(
                    ErrorCode::NotFound,
                    format!(
                        "This dashboard has a panel for `{}` and this project has no saved analysis by that name.",
                        panel.analysis_id
                    ),
                ));
            }
            if panel.width == 0 || panel.height == 0 {
                return Err(TallyOwlError::invalid_argument(format!(
                    "The panel for `{}` has no size.",
                    panel.analysis_id
                )));
            }
            if panel.column + panel.width > COLUMNS {
                return Err(TallyOwlError::invalid_argument(format!(
                    "The panel for `{}` starts at column {} and is {} wide, which runs past the {COLUMNS} the grid has.",
                    panel.analysis_id, panel.column, panel.width
                )));
            }
        }
        self.dashboards.insert(
            (dashboard.project_id, dashboard.dashboard_id.clone()),
            dashboard,
        );
        Ok(())
    }

    pub fn dashboard(&self, project_id: [u8; 16], dashboard_id: &str) -> Option<&Dashboard> {
        self.dashboards.get(&(project_id, dashboard_id.to_string()))
    }

    pub fn dashboards(&self, project_id: [u8; 16]) -> Vec<&Dashboard> {
        self.dashboards
            .iter()
            .filter(|((project, _), _)| *project == project_id)
            .map(|(_, dashboard)| dashboard)
            .collect()
    }

    /// What a panel is titled: its own title, or the analysis's name.
    pub fn title_of(&self, project_id: [u8; 16], panel: &Panel) -> String {
        panel.title.clone().unwrap_or_else(|| {
            self.analysis(project_id, &panel.analysis_id)
                .map(|analysis| analysis.name.clone())
                .unwrap_or_else(|| panel.analysis_id.clone())
        })
    }
}

// ---------------------------------------------------------------------------
// The wire surface
// ---------------------------------------------------------------------------

/// The head's saved analyses and dashboards, behind a lock.
///
/// In memory, like [`crate::policy::PolicyService`], and for the same reason
/// and with the same limitation: a restart loses them. See L106.
pub struct SavedService {
    store: Option<std::sync::Arc<tallyowl_store::SegmentedStore>>,
    held: std::sync::Mutex<Saved>,
    speaks_version: u64,
}

impl Default for SavedService {
    fn default() -> SavedService {
        SavedService::new(crate::query::ALGEBRA_VERSION)
    }
}

impl SavedService {
    /// A set that nothing outlives. For a test that is not about durability.
    pub fn new(speaks_version: u64) -> SavedService {
        SavedService {
            store: None,
            held: std::sync::Mutex::new(Saved::new()),
            speaks_version,
        }
    }

    /// The installation's saved analyses and dashboards, read back from its
    /// catalog.
    ///
    /// **The analyses load before the dashboards**, because a dashboard refuses
    /// a panel whose analysis is not there and the two are stored separately. A
    /// record that will not load is skipped and named rather than refused, for
    /// the same reason [`crate::policy::PolicyService::open`] gives: a head that
    /// would not start because one stored record was unreadable is a head an
    /// upgrade could brick.
    pub fn open(
        store: std::sync::Arc<tallyowl_store::SegmentedStore>,
        speaks_version: u64,
    ) -> (SavedService, Vec<String>) {
        let mut saved = Saved::new();
        let mut refused = Vec::new();
        let projects = match store.catalog().projects() {
            Ok(projects) => projects,
            Err(e) => {
                refused.push(format!("The stored projects could not be read: {e}"));
                Vec::new()
            }
        };

        for project in &projects {
            match store.catalog().analyses(project.project_id) {
                Err(e) => refused.push(format!(
                    "The saved analyses of `{}` could not be read: {e}",
                    project.name
                )),
                Ok(records) => {
                    for record in records {
                        if let Err(e) = saved.put_analysis(from_analysis(&record), speaks_version) {
                            refused.push(format!(
                                "The saved analysis `{}` was not loaded: {}",
                                record.analysis_id, e.message
                            ));
                        }
                    }
                }
            }
        }
        for project in &projects {
            match store.catalog().dashboards(project.project_id) {
                Err(e) => refused.push(format!(
                    "The dashboards of `{}` could not be read: {e}",
                    project.name
                )),
                Ok(records) => {
                    for record in records {
                        if let Err(e) = saved.put_dashboard(from_dashboard(&record)) {
                            refused.push(format!(
                                "The dashboard `{}` was not loaded: {}",
                                record.dashboard_id, e.message
                            ));
                        }
                    }
                }
            }
        }
        (
            SavedService {
                store: Some(store),
                held: std::sync::Mutex::new(saved),
                speaks_version,
            },
            refused,
        )
    }

    pub fn put_analysis(
        &self,
        wire: &tallyowl_control_api::types::SavedAnalysis,
        by: &str,
    ) -> Result<tallyowl_control_api::types::SavedAnalysis, TallyOwlError> {
        let now = tallyowl_obs::time::now_ms();
        let project_id = to_id(&wire.project_id)?;
        let mut held = self.held.lock().expect("saved");
        let created_at = held
            .analysis(project_id, &wire.analysis_id)
            .map(|held| held.created_at)
            .unwrap_or(now);
        let analysis = Analysis {
            analysis_id: wire.analysis_id.clone(),
            project_id,
            name: wire.name.clone(),
            form: form_name(&wire.form).to_string(),
            request: wire.request.clone(),
            algebra_version: wire.algebra_version,
            created_at,
            updated_at: now,
            updated_by: by.to_string(),
        };
        // Checked before it is stored, and stored before it is held. An
        // analysis that a person saw saved and that did not survive is worse
        // than one that refused.
        held.put_analysis(analysis.clone(), self.speaks_version)?;
        if let Some(store) = &self.store {
            if let Err(e) = store.catalog().put_analysis(&to_analysis(&analysis)) {
                // Take it back out, so what is held and what is stored agree.
                let _ = held.remove_analysis(project_id, &analysis.analysis_id);
                return Err(TallyOwlError::unavailable(format!(
                    "This analysis was not stored, so it was not saved either: {e}"
                )));
            }
        }
        Ok(to_wire_analysis(&analysis, &wire.form))
    }

    pub fn list_analyses(
        &self,
        project_id: [u8; 16],
    ) -> tallyowl_control_api::types::SavedAnalysisList {
        let held = self.held.lock().expect("saved");
        tallyowl_control_api::types::SavedAnalysisList {
            analyses: held
                .analyses(project_id)
                .into_iter()
                .map(|analysis| to_wire_analysis(analysis, &form_of(&analysis.form)))
                .collect(),
            next_cursor: None,
        }
    }

    pub fn remove_analysis(
        &self,
        project_id: [u8; 16],
        analysis_id: &str,
    ) -> Result<(), TallyOwlError> {
        let mut held = self.held.lock().expect("saved");
        // The refusal comes first: an analysis a dashboard shows is not removed,
        // and a removal that had already touched the catalog would have to be
        // undone.
        held.remove_analysis(project_id, analysis_id)?;
        if let Some(store) = &self.store {
            store
                .catalog()
                .remove_analysis(project_id, analysis_id)
                .map_err(|e| {
                    TallyOwlError::unavailable(format!(
                        "This analysis was taken out of this head and not out of the catalog, so it will come back on a restart: {e}"
                    ))
                })?;
        }
        Ok(())
    }

    pub fn put_dashboard(
        &self,
        wire: &tallyowl_control_api::types::SavedDashboard,
        by: &str,
    ) -> Result<tallyowl_control_api::types::SavedDashboard, TallyOwlError> {
        let dashboard = Dashboard {
            dashboard_id: wire.dashboard_id.clone(),
            project_id: to_id(&wire.project_id)?,
            name: wire.name.clone(),
            panels: wire
                .panels
                .iter()
                .map(|panel| Panel {
                    analysis_id: panel.analysis_id.clone(),
                    title: panel.title.clone(),
                    column: panel.column as u32,
                    row: panel.row as u32,
                    width: panel.width as u32,
                    height: panel.height as u32,
                })
                .collect(),
            updated_at: tallyowl_obs::time::now_ms(),
            updated_by: by.to_string(),
        };
        let mut held = self.held.lock().expect("saved");
        held.put_dashboard(dashboard.clone())?;
        if let Some(store) = &self.store {
            if let Err(e) = store.catalog().put_dashboard(&to_dashboard(&dashboard)) {
                return Err(TallyOwlError::unavailable(format!(
                    "This dashboard was not stored, so it was not saved either: {e}"
                )));
            }
        }
        Ok(to_wire_dashboard(&dashboard))
    }

    /// Remove one dashboard, and the stored copy with it.
    pub fn remove_dashboard(
        &self,
        project_id: [u8; 16],
        dashboard_id: &str,
    ) -> Result<(), TallyOwlError> {
        let mut held = self.held.lock().expect("saved");
        held.remove_dashboard(project_id, dashboard_id)?;
        if let Some(store) = &self.store {
            if let Err(e) = store.catalog().remove_dashboard(project_id, dashboard_id) {
                return Err(TallyOwlError::unavailable(format!(
                    "This dashboard was not removed from storage, so it will come back: {e}"
                )));
            }
        }
        Ok(())
    }

    pub fn list_dashboards(
        &self,
        project_id: [u8; 16],
    ) -> tallyowl_control_api::types::SavedDashboardList {
        let held = self.held.lock().expect("saved");
        tallyowl_control_api::types::SavedDashboardList {
            dashboards: held
                .dashboards(project_id)
                .into_iter()
                .map(to_wire_dashboard)
                .collect(),
            next_cursor: None,
        }
    }
}

fn to_id(bytes: &[u8]) -> Result<[u8; 16], TallyOwlError> {
    bytes.try_into().map_err(|_| {
        TallyOwlError::invalid_argument(format!(
            "A project identifier is 16 bytes and this one is {}.",
            bytes.len()
        ))
    })
}

fn to_wire_analysis(
    analysis: &Analysis,
    form: &tallyowl_control_api::types::QueryForm,
) -> tallyowl_control_api::types::SavedAnalysis {
    tallyowl_control_api::types::SavedAnalysis {
        analysis_id: analysis.analysis_id.clone(),
        project_id: analysis.project_id.to_vec(),
        name: analysis.name.clone(),
        form: form.clone(),
        request: analysis.request.clone(),
        algebra_version: analysis.algebra_version,
        created_at: Some(analysis.created_at),
        updated_at: Some(analysis.updated_at),
        updated_by: (!analysis.updated_by.is_empty()).then(|| analysis.updated_by.clone()),
    }
}

fn to_wire_dashboard(dashboard: &Dashboard) -> tallyowl_control_api::types::SavedDashboard {
    tallyowl_control_api::types::SavedDashboard {
        dashboard_id: dashboard.dashboard_id.clone(),
        project_id: dashboard.project_id.to_vec(),
        name: dashboard.name.clone(),
        panels: dashboard
            .panels
            .iter()
            .map(|panel| tallyowl_control_api::types::DashboardPanel {
                analysis_id: panel.analysis_id.clone(),
                title: panel.title.clone(),
                column: panel.column as u64,
                row: panel.row as u64,
                width: panel.width as u64,
                height: panel.height as u64,
            })
            .collect(),
        updated_at: Some(dashboard.updated_at),
        updated_by: (!dashboard.updated_by.is_empty()).then(|| dashboard.updated_by.clone()),
    }
}

fn form_name(form: &tallyowl_control_api::types::QueryForm) -> &'static str {
    use tallyowl_control_api::types::QueryForm as F;
    match form {
        F::Node => "node",
        F::Funnel => "funnel",
        F::Retention => "retention",
        F::Path => "path",
        F::Trace => "trace",
        F::Timeline => "timeline",
        F::Attribution => "attribution",
        F::CampaignSummary => "campaign-summary",
    }
}

fn form_of(name: &str) -> tallyowl_control_api::types::QueryForm {
    use tallyowl_control_api::types::QueryForm as F;
    match name {
        "funnel" => F::Funnel,
        "retention" => F::Retention,
        "path" => F::Path,
        "trace" => F::Trace,
        "timeline" => F::Timeline,
        "attribution" => F::Attribution,
        "campaign-summary" => F::CampaignSummary,
        _ => F::Node,
    }
}

// ---------------------------------------------------------------------------
// The catalog's shapes
// ---------------------------------------------------------------------------

fn to_analysis(analysis: &Analysis) -> tallyowl_store::control::AnalysisRecord {
    tallyowl_store::control::AnalysisRecord {
        analysis_id: analysis.analysis_id.clone(),
        project_id: analysis.project_id,
        name: analysis.name.clone(),
        form: analysis.form.clone(),
        request: analysis.request.clone(),
        algebra_version: analysis.algebra_version,
        created_at: analysis.created_at,
        updated_at: analysis.updated_at,
        updated_by: analysis.updated_by.clone(),
    }
}

fn from_analysis(record: &tallyowl_store::control::AnalysisRecord) -> Analysis {
    Analysis {
        analysis_id: record.analysis_id.clone(),
        project_id: record.project_id,
        name: record.name.clone(),
        form: record.form.clone(),
        request: record.request.clone(),
        algebra_version: record.algebra_version,
        created_at: record.created_at,
        updated_at: record.updated_at,
        updated_by: record.updated_by.clone(),
    }
}

fn to_dashboard(dashboard: &Dashboard) -> tallyowl_store::control::DashboardRecord {
    tallyowl_store::control::DashboardRecord {
        dashboard_id: dashboard.dashboard_id.clone(),
        project_id: dashboard.project_id,
        name: dashboard.name.clone(),
        panels: dashboard
            .panels
            .iter()
            .map(|panel| tallyowl_store::control::PanelRecord {
                analysis_id: panel.analysis_id.clone(),
                title: panel.title.clone(),
                column: panel.column,
                row: panel.row,
                width: panel.width,
                height: panel.height,
            })
            .collect(),
        updated_at: dashboard.updated_at,
        updated_by: dashboard.updated_by.clone(),
    }
}

fn from_dashboard(record: &tallyowl_store::control::DashboardRecord) -> Dashboard {
    Dashboard {
        dashboard_id: record.dashboard_id.clone(),
        project_id: record.project_id,
        name: record.name.clone(),
        panels: record
            .panels
            .iter()
            .map(|panel| Panel {
                analysis_id: panel.analysis_id.clone(),
                title: panel.title.clone(),
                column: panel.column,
                row: panel.row,
                width: panel.width,
                height: panel.height,
            })
            .collect(),
        updated_at: record.updated_at,
        updated_by: record.updated_by.clone(),
    }
}
