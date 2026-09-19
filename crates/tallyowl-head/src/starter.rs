//! The starter campaign dashboard.
//!
//! `docs/PLAN.md` Phase 9 asks for "campaign, conversion, value, cost, and
//! return dashboards". The operator and the rendering answer all five, and
//! without this a fresh installation has the capability and an empty screen.
//! Every operator would hand-build the same panels.
//!
//! # It is offered once, and a delete sticks
//!
//! The mark is in the control catalog under `control/seeded/<project>`, and it
//! says **"this was offered"** rather than "this exists". The difference is the
//! whole point: an operator who deletes the starter dashboard must not find it
//! back after the next restart or the next upgrade. A check for the dashboard
//! itself would do exactly that.
//!
//! # It is ordinary saved state, not a special case
//!
//! The panels are ordinary saved analyses and the dashboard is an ordinary
//! saved dashboard, written through the same service an operator writes
//! through. Nothing reads a "starter" flag at query time, nothing refuses to
//! edit them, and an erasure or a restore treats them like anything else in the
//! catalog. They are a first draft somebody can change, not a fixture.
//!
//! # Why these six panels
//!
//! They are the questions a person opens a campaign report to ask, in the order
//! they ask them: what did each campaign earn, which channels brought people,
//! what did the money come from, and what did each model say. The last one
//! matters more than it looks — two models disagree about the same traffic on
//! purpose, and a report that showed only one would hide the disagreement.

use tallyowl_control_api::types::{
    AttributionModel, CampaignSummaryQuery, CampaignSummaryQuery_dimension as SummaryDimension,
    DashboardPanel, QueryForm, QueryRequest, SavedAnalysis, SavedDashboard, TimeRange,
};

/// The identifier the starter dashboard takes.
pub const DASHBOARD_ID: &str = "starter-campaigns";

/// Who the catalog records as having written it.
pub const AUTHOR: &str = "tallyowl";

/// How far back the starter panels look.
///
/// Thirty days, which is the shipped attribution lookback, so the range a panel
/// covers and the window it attributes over are the same number. A panel that
/// looked back further than the model would show touches that could never earn
/// anything in it.
const RANGE_MS: i64 = 30 * 86_400_000;

/// One starter panel: what it asks and where it sits.
struct Starter {
    id: &'static str,
    name: &'static str,
    dimension: SummaryDimension,
    model: AttributionModel,
    column: u64,
    row: u64,
    width: u64,
}

/// The six panels, in the order a person reads them.
const PANELS: &[Starter] = &[
    Starter {
        id: "starter-campaign-return",
        name: "Campaigns: value, cost, and return",
        dimension: SummaryDimension::Campaign,
        model: AttributionModel::LastNonDirect,
        column: 0,
        row: 0,
        width: 12,
    },
    Starter {
        id: "starter-channel-mix",
        name: "Channels: where people came from",
        dimension: SummaryDimension::Channel,
        model: AttributionModel::LastNonDirect,
        column: 0,
        row: 1,
        width: 6,
    },
    Starter {
        id: "starter-source-mix",
        name: "Sources: which sites and platforms",
        dimension: SummaryDimension::Source,
        model: AttributionModel::LastNonDirect,
        column: 6,
        row: 1,
        width: 6,
    },
    // The three that disagree, side by side. First-touch credits discovery,
    // last-non-direct credits the decision, and linear splits it. Showing one
    // of them alone is how a marketing argument gets settled by whichever
    // report somebody opened.
    Starter {
        id: "starter-model-first",
        name: "Credit by first touch",
        dimension: SummaryDimension::Campaign,
        model: AttributionModel::FirstTouch,
        column: 0,
        row: 2,
        width: 4,
    },
    Starter {
        id: "starter-model-last-non-direct",
        name: "Credit by last non-direct touch",
        dimension: SummaryDimension::Campaign,
        model: AttributionModel::LastNonDirect,
        column: 4,
        row: 2,
        width: 4,
    },
    Starter {
        id: "starter-model-linear",
        name: "Credit shared across every touch",
        dimension: SummaryDimension::Campaign,
        model: AttributionModel::Linear,
        column: 8,
        row: 2,
        width: 4,
    },
];

/// What the starter dashboard is made of, for one project.
///
/// The range is relative to `now`, so a project seeded today and one seeded
/// next year both get a panel covering their own last thirty days.
pub fn analyses(project_id: [u8; 16], goal: &str, now: i64) -> Vec<SavedAnalysis> {
    PANELS
        .iter()
        .map(|panel| {
            let request = QueryRequest {
                algebra_version: crate::query::ALGEBRA_VERSION,
                consistency: tallyowl_control_api::types::Consistency::Committed,
                max_staleness_ms: None,
                budget: None,
                // A dashboard that quietly drew a smaller number would be the
                // failure FAILURE_MODES.md section 2 ranks worst.
                allow_partial: false,
                comparison_range: None,
                form: QueryForm::CampaignSummary,
                node: None,
                funnel: None,
                retention: None,
                path: None,
                trace: None,
                timeline: None,
                attribution: None,
                campaign_summary: Some(CampaignSummaryQuery {
                    project_id: project_id.to_vec(),
                    range: TimeRange {
                        range_start: now - RANGE_MS,
                        range_end: now,
                        basis: tallyowl_control_api::types::TimeBasis::OccurredAt,
                        timezone: None,
                    },
                    conversion_goal: goal.to_string(),
                    model: panel.model.clone(),
                    lookback_ms: RANGE_MS,
                    dimension: Some(panel.dimension.clone()),
                    touch_filter: None,
                    resolution: None,
                }),
            };
            SavedAnalysis {
                analysis_id: panel.id.to_string(),
                project_id: project_id.to_vec(),
                name: panel.name.to_string(),
                form: QueryForm::CampaignSummary,
                request: tallyowl_control_api::codec::encode_query_request(&request),
                algebra_version: crate::query::ALGEBRA_VERSION,
                created_at: Some(now),
                updated_at: Some(now),
                updated_by: Some(AUTHOR.to_string()),
            }
        })
        .collect()
}

/// The dashboard that shows them.
pub fn dashboard(project_id: [u8; 16], now: i64) -> SavedDashboard {
    SavedDashboard {
        dashboard_id: DASHBOARD_ID.to_string(),
        project_id: project_id.to_vec(),
        name: "Campaigns".to_string(),
        panels: PANELS
            .iter()
            .map(|panel| DashboardPanel {
                analysis_id: panel.id.to_string(),
                // Absent takes the analysis's own name, so a panel cannot
                // silently disagree with the thing it shows.
                title: None,
                column: panel.column,
                row: panel.row,
                width: panel.width,
                height: 1,
            })
            .collect(),
        updated_at: Some(now),
        updated_by: Some(AUTHOR.to_string()),
    }
}

/// Offer the starter dashboard to every project that has not been offered it.
///
/// Returns the projects it wrote one for. A failure for one project is named
/// and the rest still get theirs: a starter dashboard is a convenience, and a
/// head that would not start because one could not be written would be trading
/// a whole installation for a convenience.
pub fn seed(
    store: &std::sync::Arc<tallyowl_store::SegmentedStore>,
    saved: &crate::saved::SavedService,
    goal: &str,
    now: i64,
) -> (Vec<String>, Vec<String>) {
    let mut seeded = Vec::new();
    let mut refused = Vec::new();

    let projects = match store.catalog().projects() {
        Ok(projects) => projects,
        Err(e) => {
            refused.push(format!(
                "The projects could not be read, so no starter dashboard was written: {e}"
            ));
            return (seeded, refused);
        }
    };

    for project in projects {
        match store.catalog().was_seeded(project.project_id) {
            Err(e) => {
                refused.push(format!(
                    "Whether `{}` already has a starter dashboard could not be read: {e}",
                    project.name
                ));
                continue;
            }
            // Offered once. An operator who deleted it does not get it back.
            Ok(true) => continue,
            Ok(false) => {}
        }

        let written = (|| -> Result<(), tallyowl_obs::error::TallyOwlError> {
            for analysis in analyses(project.project_id, goal, now) {
                saved.put_analysis(&analysis, AUTHOR)?;
            }
            saved.put_dashboard(&dashboard(project.project_id, now), AUTHOR)?;
            Ok(())
        })();

        match written {
            Err(e) => refused.push(format!(
                "The starter dashboard for `{}` was not written: {}",
                project.name, e.message
            )),
            Ok(()) => {
                // The mark goes last. A crash between the dashboard and the
                // mark offers it again, which is a duplicate write of the same
                // identifiers; a mark that went first could lose the dashboard
                // entirely and never offer it again.
                if let Err(e) = store.catalog().mark_seeded(project.project_id, now) {
                    refused.push(format!(
                        "The starter dashboard for `{}` was written and not marked, so it may be offered again: {e}",
                        project.name
                    ));
                }
                seeded.push(project.name.clone());
            }
        }
    }
    (seeded, refused)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_directory(name: &str) -> std::path::PathBuf {
        let place = std::env::temp_dir().join(format!("tallyowl-starter-{name}"));
        let _ = std::fs::remove_dir_all(&place);
        std::fs::create_dir_all(&place).expect("a directory");
        place
    }

    fn an_installation(
        name: &str,
    ) -> (
        std::sync::Arc<tallyowl_store::SegmentedStore>,
        crate::saved::SavedService,
        [u8; 16],
    ) {
        let store = std::sync::Arc::new(
            tallyowl_store::SegmentedStore::open(a_directory(name)).expect("the store opens"),
        );
        let workspace = tallyowl_store::control::Workspace {
            workspace_id: [1; 16],
            name: "home".into(),
            created_at: 0,
        };
        let project = tallyowl_store::control::Project {
            project_id: [2; 16],
            workspace_id: workspace.workspace_id,
            name: "local".into(),
            description: None,
            created_at: 0,
        };
        store.catalog().put_workspace(&workspace).unwrap();
        store.catalog().put_project(&project).unwrap();
        let saved = crate::saved::SavedService::open(
            std::sync::Arc::clone(&store),
            crate::query::ALGEBRA_VERSION,
        )
        .0;
        (store, saved, project.project_id)
    }

    #[test]
    fn a_new_project_gets_a_dashboard_it_can_open() {
        let (store, saved, project) = an_installation("new");
        let (seeded, refused) = seed(&store, &saved, "purchase", 1_000_000);
        assert_eq!(seeded, vec!["local".to_string()]);
        assert!(refused.is_empty(), "{refused:?}");

        let dashboards = saved.list_dashboards(project);
        assert_eq!(dashboards.dashboards.len(), 1);
        assert_eq!(dashboards.dashboards[0].panels.len(), PANELS.len());

        // Every panel points at an analysis that exists. A dashboard whose
        // panel names nothing is refused when it is saved, so this also proves
        // the analyses were written first.
        let analyses = saved.list_analyses(project);
        assert_eq!(analyses.analyses.len(), PANELS.len());
        for panel in &dashboards.dashboards[0].panels {
            assert!(
                analyses
                    .analyses
                    .iter()
                    .any(|a| a.analysis_id == panel.analysis_id),
                "the panel `{}` names an analysis that is not there",
                panel.analysis_id
            );
        }
    }

    #[test]
    fn deleting_the_starter_dashboard_sticks() {
        // The reason the mark says "offered" rather than "exists". An operator
        // who does not want it must not find it back after a restart.
        let (store, saved, project) = an_installation("deleted");
        seed(&store, &saved, "purchase", 1_000_000);

        saved
            .remove_dashboard(project, DASHBOARD_ID)
            .expect("an operator removes it");
        assert_eq!(saved.list_dashboards(project).dashboards.len(), 0);

        // A second start-up, with the same catalog.
        let (seeded, refused) = seed(&store, &saved, "purchase", 2_000_000);
        assert!(seeded.is_empty(), "it was offered a second time");
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(saved.list_dashboards(project).dashboards.len(), 0);
    }

    #[test]
    fn every_starter_panel_holds_a_query_this_build_answers() {
        // A panel that saved a request the head refuses would be a screen that
        // reads "this installation cannot answer that" on first run.
        let now = 1_000_000;
        for analysis in analyses([2; 16], "purchase", now) {
            let decoded = tallyowl_control_api::codec::decode_query_request(&analysis.request)
                .expect("the saved request reads back");
            assert_eq!(decoded.form, QueryForm::CampaignSummary);
            assert_eq!(decoded.algebra_version, crate::query::ALGEBRA_VERSION);
            assert!(!decoded.allow_partial);
            let summary = decoded.campaign_summary.expect("a campaign question");
            // The range a panel covers and the window it attributes over are
            // the same number, or the panel shows touches that could never earn
            // anything in it.
            assert_eq!(
                summary.range.range_end - summary.range.range_start,
                summary.lookback_ms
            );
        }
    }

    #[test]
    fn the_panels_fit_the_grid() {
        // A width past the grid is refused when the dashboard is saved, so a
        // starter that did not fit would fail on first run rather than at
        // review.
        for panel in PANELS {
            assert!(
                panel.column + panel.width <= crate::saved::COLUMNS as u64,
                "`{}` runs past the twelve-column grid",
                panel.id
            );
        }
    }
}
