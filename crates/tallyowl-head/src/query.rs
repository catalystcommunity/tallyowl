//! The query executor.
//!
//! `docs/QUERY.md` owns the algebra. This module executes the part of it that
//! alpha answers: scan, filter, project, aggregate, sort, limit, and union,
//! over the `events` dataset. It refuses everything else **by name**, so a
//! caller learns what is missing rather than getting a wrong answer or a
//! decoder failure.
//!
//! Three rules from D18 and D21 hold here, and all three are about correctness
//! rather than completeness:
//!
//! - **correctness is the default.** A query that cannot see all of its data
//!   returns `incomplete-result` unless the caller explicitly asked for a
//!   partial answer. It never quietly returns a smaller number;
//! - **every result says whether it is exact.** An exact measure that reaches
//!   its cap fails and names the approximate measure that would answer, rather
//!   than becoming approximate on its own;
//! - **a count is over logical events.** A physical duplicate can exist after a
//!   pathological retry or a manual replay. Deduplication happens once, above
//!   the scan, so every operator over it sees logical rows. See DELIVERY.md
//!   section 6.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use tallyowl_control_api::codec::decode_query_node_box;
use tallyowl_control_api::types::{
    AggregateNode, Consistency, Dataset, Dimension, Exactness, LimitNode, Measure, MeasureKind,
    ProjectNode, QueryForm, QueryNodeBox, QueryNodeKind, QueryRequest, QueryResponse,
    ResultMetadata, ResultRow, ScanNode, SortNode, TimeBasis as WireBasis, UnionNode,
};
use tallyowl_obs::error::TallyOwlError;
use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_store::{Store, TimeBasis};

use tallyowl_store::cbor::{
    decode as cbor_decode, encode as cbor_encode, MapBuilder, Value as CborValue,
};

use crate::calendar::{self, Zone};
use crate::expr::{self, Prepared};

/// The algebra version this head speaks.
pub const ALGEBRA_VERSION: u64 = 1;

/// How many distinct values an exact `count_distinct` holds before it refuses.
///
/// QUERY.md section 7: an exact measure that reaches its cap fails with a typed
/// error naming the approximate measure the caller can select. It never becomes
/// approximate on its own, because D21 forbids that.
pub const DISTINCT_CAP: usize = 100_000;

pub struct QueryService {
    pub store: Arc<dyn Store>,
    pub max_runtime_ms: i64,
    pub max_expression_depth: u32,
    /// What a domain operator refuses to exceed. `docs/QUERY.md` section 14
    /// makes the budget the smaller of the request's and the policy's, and
    /// these are the policy's.
    pub guards: crate::analysis::Guards,
    /// The attribution weights and windows of each project. Phase 9, D40.
    ///
    /// It is here rather than passed in with each question because a question
    /// names a model and a window and never the weights: the weights are the
    /// operator's configuration, and a caller that could send them could make
    /// one campaign look better than another by asking differently.
    pub attribution: Arc<crate::attribution::AttributionService>,
    /// The collection policy, for the one thing a query reads from it: whether
    /// attribution needs marketing consent. D30 puts that decision in the
    /// applicable policy, because TallyOwl is not the policy authority for an
    /// application.
    pub policy: Arc<crate::policy::PolicyService>,
    /// The materialised identity graph of each project. L103, L135.
    ///
    /// Without it every question that resolves identity read the whole history
    /// of the installation, because an `identify` from last year is what binds
    /// this month's anonymous events. The cost grew with the installation's age
    /// rather than with the question.
    pub identity: Arc<crate::identity::IdentityCache>,
}

/// What one operator produced.
///
/// A scan and a filter produce stored rows, because the operators above them
/// read fields. Everything else produces a table of named columns, because a
/// caller reads those.
enum Stage {
    Rows {
        rows: Vec<EventRow>,
        basis: TimeBasis,
        incomplete: bool,
        /// The timezone the scan's range named, carried so that a calendar
        /// interval above it groups by the periods the reader means. L134.
        zone: Zone,
    },
    Table(Table),
}

struct Table {
    columns: Vec<String>,
    rows: Vec<Vec<PropertyValue>>,
    exactness: Vec<Exactness>,
    incomplete: bool,
}

impl Stage {
    fn incomplete(&self) -> bool {
        match self {
            Stage::Rows { incomplete, .. } => *incomplete,
            Stage::Table(table) => table.incomplete,
        }
    }
}

impl QueryService {
    pub fn run(&self, request: QueryRequest) -> Result<QueryResponse, TallyOwlError> {
        let started = Instant::now();

        if request.algebra_version > ALGEBRA_VERSION {
            return Err(TallyOwlError::new(
                tallyowl_obs::ErrorCode::SchemaUnsupported,
                format!(
                    "This query was written for a newer version of TallyOwl. It asks for query version {}, and this installation speaks version {ALGEBRA_VERSION}.",
                    request.algebra_version
                ),
            ));
        }

        match request.form {
            QueryForm::Trace => {
                let trace = request.trace.as_ref().ok_or_else(|| missing("a trace"))?;
                return self.trace(trace, &request);
            }
            QueryForm::Funnel => {
                let funnel = request.funnel.as_ref().ok_or_else(|| missing("a funnel"))?;
                return self.funnel(funnel, &request);
            }
            QueryForm::Retention => {
                let retention = request
                    .retention
                    .as_ref()
                    .ok_or_else(|| missing("a retention question"))?;
                return self.retention(retention, &request);
            }
            QueryForm::Path => {
                let path = request.path.as_ref().ok_or_else(|| missing("a path"))?;
                return self.path(path, &request);
            }
            QueryForm::Timeline => {
                let timeline = request
                    .timeline
                    .as_ref()
                    .ok_or_else(|| missing("a timeline"))?;
                return self.timeline(timeline, &request);
            }
            QueryForm::Attribution => {
                let attribution = request
                    .attribution
                    .as_ref()
                    .ok_or_else(|| missing("an attribution question"))?;
                return self.attribution(attribution, &request);
            }
            QueryForm::CampaignSummary => {
                let summary = request
                    .campaign_summary
                    .as_ref()
                    .ok_or_else(|| missing("a campaign report"))?;
                return self.campaign_summary(summary, &request);
            }
            QueryForm::Node => {}
        }
        let encoded = request.node.as_deref().ok_or_else(|| {
            TallyOwlError::invalid_argument(
                "This request says it carries a query and carries none. Send the query.",
            )
        })?;

        let stage = self.execute(&child(encoded)?, &request, started)?;
        if stage.incomplete() && !request.allow_partial {
            // Correctness first. A caller must ask for a partial result.
            return Err(TallyOwlError::new(
                tallyowl_obs::ErrorCode::IncompleteResult,
                self.incomplete_message(),
            ));
        }
        let table = self.materialize(stage);

        if let Some(budget) = &request.budget {
            if let Some(most) = budget.max_rows {
                if table.rows.len() as u64 > most {
                    return Err(TallyOwlError::new(
                        tallyowl_obs::ErrorCode::BudgetExceeded,
                        format!(
                            "This query produced {} rows and the budget allows {most}. Add a limit, or raise the budget.",
                            table.rows.len()
                        ),
                    )
                    .retryable(false));
                }
            }
        }

        Ok(QueryResponse {
            columns: table.columns,
            rows: table
                .rows
                .iter()
                .map(|values| ResultRow {
                    values: values.iter().map(to_wire).collect(),
                })
                .collect(),
            metadata: ResultMetadata {
                algebra_version: ALGEBRA_VERSION,
                commit_watermark: self.store.commit_watermark(),
                freshness_ms: 0,
                complete: !table.incomplete,
                missing: None,
                exactness: table.exactness,
                scanned_bytes: 0,
                scanned_segments: 1,
                cold_bytes: None,
                tombstone_generation: 0,
                applied_retention_class: None,
                warnings: None,
                next_cursor: None,
            },
        })
    }

    /// Assemble one trace: every span it holds, and every error linked to it.
    ///
    /// A trace routes by its trace ID, so one tablet already holds all of it.
    /// The lookup is the exact high-cardinality one the locator exists for, and
    /// no time range is needed because the trace ID is the key. See D16 and D35.
    ///
    /// The result is a waterfall in the order a person reads it: a span comes
    /// after its parent, and siblings come in start order. The depth is a
    /// column, so a caller draws the tree without walking it again.
    fn trace(
        &self,
        trace: &tallyowl_control_api::types::TraceQuery,
        request: &QueryRequest,
    ) -> Result<QueryResponse, TallyOwlError> {
        let project_id = to_id(&trace.project_id)?;
        let trace_id: [u8; 16] = trace
            .trace_id
            .as_slice()
            .try_into()
            .map_err(|_| TallyOwlError::invalid_argument("A trace identifier is 16 bytes."))?;

        let found = self
            .store
            .lookup_correlated(tallyowl_store::segment::schema::TRACE_ID, &trace_id)
            .map_err(crate::ingest::to_service_error)?;

        if found.incomplete && !request.allow_partial {
            return Err(TallyOwlError::new(
                tallyowl_obs::ErrorCode::IncompleteResult,
                format!(
                    "We could not read all of the stored data for this trace, so it would be missing spans. Ask for a partial result if an incomplete trace is useful.{}",
                    self.damage()
                ),
            ));
        }

        // Tenancy is checked here and not left to the lookup. A trace ID is
        // supplied by a caller, and one project must never assemble another
        // project's trace by guessing an identifier.
        let mut rows: Vec<EventRow> = found
            .rows
            .into_iter()
            .filter(|row| row.project_id == project_id)
            .collect();
        let mut seen: BTreeSet<[u8; 16]> = BTreeSet::new();
        rows.retain(|row| seen.insert(row.event_id));

        let ordered = waterfall(&rows);
        let columns = vec![
            "depth".to_string(),
            "span_id".to_string(),
            "parent_span_id".to_string(),
            "kind".to_string(),
            "operation".to_string(),
            "service_name".to_string(),
            "start_at".to_string(),
            "duration_ms".to_string(),
            "status".to_string(),
            "error_event_id".to_string(),
        ];
        let table_rows: Vec<Vec<PropertyValue>> = ordered
            .iter()
            .map(|(depth, row)| {
                let property = |key: &str| {
                    row.properties
                        .get(key)
                        .map(|(value, _)| value.clone())
                        .unwrap_or(PropertyValue::Null)
                };
                vec![
                    PropertyValue::Unsigned(*depth as u64),
                    property("span_id"),
                    property("parent_span_id"),
                    PropertyValue::Text(row.kind.clone()),
                    PropertyValue::Text(row.name.clone()),
                    row.service_name
                        .clone()
                        .map(PropertyValue::Text)
                        .unwrap_or(PropertyValue::Null),
                    PropertyValue::Integer(row.occurred_at),
                    property("duration_ms"),
                    property("status"),
                    property("error_event_id"),
                ]
            })
            .collect();

        Ok(QueryResponse {
            columns,
            rows: table_rows
                .iter()
                .map(|values| ResultRow {
                    values: values.iter().map(to_wire).collect(),
                })
                .collect(),
            metadata: ResultMetadata {
                algebra_version: ALGEBRA_VERSION,
                commit_watermark: self.store.commit_watermark(),
                freshness_ms: 0,
                complete: !found.incomplete,
                missing: None,
                exactness: Vec::new(),
                scanned_bytes: 0,
                scanned_segments: 1,
                cold_bytes: None,
                tombstone_generation: 0,
                applied_retention_class: None,
                warnings: None,
                next_cursor: None,
            },
        })
    }

    // -----------------------------------------------------------------------
    // The domain operators. `docs/QUERY.md` section 12.
    //
    // Each of these scans once, builds the identity graph from what it scanned,
    // and hands both to `crate::analysis`. The rules live there; these translate
    // the wire request into them and the result back into a table.
    //
    // **The identity graph is built from the same rows the operator counts.**
    // That is what makes a result reproducible by hand: everything the answer
    // depends on is in the range the caller named, and there is no second
    // durable copy that could disagree.
    // -----------------------------------------------------------------------

    /// Scan one project and range, and build its identity graph.
    ///
    /// The scan reaches back before the range for identity, because an
    /// `identify` that happened last month is what makes this month's anonymous
    /// events belong to somebody. Without the lookback a funnel over a week
    /// would treat every returning person as new.
    fn rows_and_identity(
        &self,
        project_id: [u8; 16],
        range: &tallyowl_control_api::types::TimeRange,
    ) -> Result<(Vec<EventRow>, bool, crate::identity::Identity), TallyOwlError> {
        let basis = to_basis(&range.basis);
        let scanned = self
            .store
            .scan(project_id, range.range_start, range.range_end, basis)
            .map_err(crate::ingest::to_service_error)?;

        // One logical event, whatever the physical rows, exactly as the general
        // scan does. A retry that duplicated a row must not make a funnel count
        // one person twice.
        let mut seen: BTreeSet<[u8; 16]> = BTreeSet::new();
        let mut rows: Vec<EventRow> = scanned
            .rows
            .into_iter()
            .filter(|row| seen.insert(row.event_id))
            .collect();

        // **The identity lookback, and what it used to cost.** An `identify`
        // that happened last month is what makes this month's anonymous events
        // belong to somebody, so a graph needs everything before the range as
        // well as the range. Reading all of it for every question made the cost
        // grow with the installation's age. L103 recorded that and L135 fixed
        // it: a materialised graph covers the old part and this reads only the
        // window between what it covers and the start of the range.
        //
        // The graph is bounded to the end of the range before it is used, so an
        // answer never depends on how fresh the materialisation is.
        let generation = self
            .store
            .tombstone_generation()
            .map_err(crate::ingest::to_service_error)?;
        let now = tallyowl_obs::time::now_ms();
        let mut history_incomplete = false;
        let materialised = self.identity.graph(
            project_id,
            generation,
            now,
            range.range_end,
            || -> Result<crate::identity::Identity, TallyOwlError> {
                let history = self
                    .store
                    .scan(
                        project_id,
                        i64::MIN / 2,
                        now,
                        tallyowl_store::TimeBasis::OccurredAt,
                    )
                    .map_err(crate::ingest::to_service_error)?;
                history_incomplete |= history.incomplete;
                let identity_rows: Vec<EventRow> = history
                    .rows
                    .into_iter()
                    .filter(|row| matches!(row.kind.as_str(), "identify" | "alias" | "group"))
                    .collect();
                Ok(crate::identity::Identity::build(project_id, &identity_rows))
            },
        )?;

        let mut identity = materialised.graph;
        // What the materialisation does not cover yet, up to the start of the
        // range. It is empty whenever the graph was built after the range
        // started, which is the ordinary case for a question about last week.
        if materialised.from < range.range_start {
            let window = self
                .store
                .scan(
                    project_id,
                    materialised.from,
                    range.range_start,
                    tallyowl_store::TimeBasis::OccurredAt,
                )
                .map_err(crate::ingest::to_service_error)?;
            history_incomplete |= window.incomplete;
            let catch_up: Vec<EventRow> = window
                .rows
                .into_iter()
                .filter(|row| matches!(row.kind.as_str(), "identify" | "alias" | "group"))
                .filter(|row| seen.insert(row.event_id))
                .collect();
            identity.apply(&catch_up);
        }
        // And the range's own rows, which the materialisation may or may not
        // have seen. `apply` skips what it already folded, so this is the same
        // graph either way.
        identity.apply(&rows);

        // A row from another project can only be here through a defect, and the
        // graph counts what it ignored so a test can prove the filter did work.
        debug_assert_eq!(identity.foreign_rows_ignored(), 0);
        rows.sort_by_key(|row| (row.occurred_at, row.event_id));
        Ok((rows, scanned.incomplete || history_incomplete, identity))
    }

    fn refuse_if_short(
        &self,
        incomplete: bool,
        request: &QueryRequest,
    ) -> Result<(), TallyOwlError> {
        if incomplete && !request.allow_partial {
            return Err(TallyOwlError::new(
                tallyowl_obs::ErrorCode::IncompleteResult,
                self.incomplete_message(),
            ));
        }
        Ok(())
    }

    fn answer(
        &self,
        columns: Vec<String>,
        rows: Vec<Vec<PropertyValue>>,
        incomplete: bool,
        warnings: Vec<String>,
    ) -> QueryResponse {
        QueryResponse {
            columns,
            rows: rows
                .iter()
                .map(|values| ResultRow {
                    values: values.iter().map(to_wire).collect(),
                })
                .collect(),
            metadata: ResultMetadata {
                algebra_version: ALGEBRA_VERSION,
                commit_watermark: self.store.commit_watermark(),
                freshness_ms: 0,
                complete: !incomplete,
                missing: None,
                // Every number a domain operator produces is exact. D21 forbids
                // becoming approximate without being asked, and none of these
                // estimate.
                exactness: Vec::new(),
                scanned_bytes: 0,
                scanned_segments: 1,
                cold_bytes: None,
                tombstone_generation: 0,
                applied_retention_class: None,
                warnings: (!warnings.is_empty()).then_some(warnings),
                next_cursor: None,
            },
        }
    }

    fn funnel(
        &self,
        funnel: &tallyowl_control_api::types::FunnelQuery,
        request: &QueryRequest,
    ) -> Result<QueryResponse, TallyOwlError> {
        use tallyowl_control_api::types::CorrelationBasis;

        let project_id = to_id(&funnel.project_id)?;
        let (rows, incomplete, identity) = self.rows_and_identity(project_id, &funnel.range)?;
        self.refuse_if_short(incomplete, request)?;

        let steps: Vec<crate::analysis::Step> = funnel
            .steps
            .iter()
            .map(|step| {
                Ok(crate::analysis::Step {
                    name: step.name.clone(),
                    matches: expr::prepare_unchecked(&step.r#match, self.max_expression_depth)?,
                    exclusion: step.exclusion.unwrap_or(false),
                })
            })
            .collect::<Result<_, TallyOwlError>>()?;

        let question = crate::analysis::FunnelQuestion {
            basis: match funnel.basis {
                CorrelationBasis::EndUser => crate::identity::Basis::EndUser,
                CorrelationBasis::Session => crate::identity::Basis::Session,
                CorrelationBasis::Group => crate::identity::Basis::Group,
            },
            // **Latest known unless the caller says otherwise, and this is the
            // whole point of a conversion funnel.** A person views the pricing
            // page anonymously, signs in, and buys. Event-time identity would
            // make those two correlation keys — an anonymous visitor who
            // vanished and a customer who appeared — and every sign-up funnel
            // would show nobody converting. The steps keep their own times;
            // what has to be one is the person. See L104.
            resolution: resolution_of(funnel.resolution.as_ref()),
            window_ms: funnel.window_ms,
            ordered: funnel.ordered,
            breakdown: funnel.breakdown.as_ref().map(|d| d.field.name.clone()),
        };

        let result = crate::analysis::funnel(
            &rows,
            incomplete,
            &identity,
            &steps,
            &question,
            &self.guards,
        )?;

        let mut columns = vec![
            "step".to_string(),
            "name".to_string(),
            "reached".to_string(),
            "median_ms".to_string(),
        ];
        let mut table: Vec<Vec<PropertyValue>> = Vec::new();
        let mut push = |dimension: Option<&str>, steps: &[crate::analysis::StepResult]| {
            for (index, step) in steps.iter().enumerate() {
                let mut values = vec![
                    PropertyValue::Unsigned(index as u64),
                    PropertyValue::Text(step.name.clone()),
                    PropertyValue::Unsigned(step.reached),
                    step.median_ms()
                        .map(PropertyValue::Integer)
                        .unwrap_or(PropertyValue::Null),
                ];
                if let Some(dimension) = dimension {
                    values.push(PropertyValue::Text(dimension.to_string()));
                }
                table.push(values);
            }
        };
        if result.by_dimension.is_empty() {
            push(None, &result.steps);
        } else {
            columns.push(
                funnel
                    .breakdown
                    .as_ref()
                    .map(|d| d.alias.clone())
                    .unwrap_or_else(|| "breakdown".to_string()),
            );
            for (value, steps) in &result.by_dimension {
                push(Some(value), steps);
            }
        }

        Ok(self.answer(
            columns,
            table,
            result.coverage.incomplete,
            coverage_warnings(&result.coverage, question.basis),
        ))
    }

    fn retention(
        &self,
        retention: &tallyowl_control_api::types::RetentionQuery,
        request: &QueryRequest,
    ) -> Result<QueryResponse, TallyOwlError> {
        use tallyowl_control_api::types::RetentionQuery_period as WirePeriod;

        let project_id = to_id(&retention.project_id)?;
        let (rows, incomplete, identity) = self.rows_and_identity(project_id, &retention.range)?;
        self.refuse_if_short(incomplete, request)?;

        let question = crate::analysis::RetentionQuestion {
            initial: expr::prepare_unchecked(&retention.initial, self.max_expression_depth)?,
            returning: expr::prepare_unchecked(&retention.returning, self.max_expression_depth)?,
            period: crate::analysis::Periods {
                unit: match retention.period {
                    WirePeriod::Day => calendar::Unit::Day,
                    WirePeriod::Week => calendar::Unit::Week,
                    WirePeriod::Month => calendar::Unit::Month,
                },
                // A cohort period is a calendar period in the zone the range
                // named, so "month" is the month the reader means. L134.
                zone: Zone::named(retention.range.timezone.as_deref())?,
            },
            periods: retention.periods as usize,
            first_time_only: retention.first_time_only,
            basis: crate::identity::Basis::EndUser,
            // Latest known unless the caller says otherwise, because a
            // retention matrix asks whether the same person came back, and a
            // person who signed in between two visits is the same person. A
            // cohort question that wants who they were at the time asks for
            // event time.
            resolution: resolution_of(retention.resolution.as_ref()),
            // Cohorts are measured from the start of the range, so two runs of
            // one query give the same matrix.
            epoch: retention.range.range_start,
        };

        let result =
            crate::analysis::retention(&rows, incomplete, &identity, &question, &self.guards)?;

        let mut columns = vec!["cohort_start".to_string(), "cohort_size".to_string()];
        for period in 0..question.periods {
            columns.push(format!("{}_{period}", result.period));
            columns.push(format!("{}_{period}_rate", result.period));
        }
        let table: Vec<Vec<PropertyValue>> = result
            .cohorts
            .iter()
            .map(|cohort| {
                let mut values = vec![
                    PropertyValue::Integer(cohort.started_at),
                    PropertyValue::Unsigned(cohort.size),
                ];
                for period in 0..question.periods {
                    values.push(PropertyValue::Unsigned(
                        cohort.returned.get(period).copied().unwrap_or(0),
                    ));
                    values.push(PropertyValue::Float(cohort.rate(period)));
                }
                values
            })
            .collect();

        Ok(self.answer(
            columns,
            table,
            result.coverage.incomplete,
            coverage_warnings(&result.coverage, crate::identity::Basis::EndUser),
        ))
    }

    fn path(
        &self,
        path: &tallyowl_control_api::types::PathQuery,
        request: &QueryRequest,
    ) -> Result<QueryResponse, TallyOwlError> {
        use tallyowl_control_api::types::PathQuery_direction as Direction;

        let project_id = to_id(&path.project_id)?;
        let (rows, incomplete, identity) = self.rows_and_identity(project_id, &path.range)?;
        self.refuse_if_short(incomplete, request)?;

        let question = crate::analysis::PathQuestion {
            anchor: expr::prepare_unchecked(&path.anchor, self.max_expression_depth)?,
            forwards: matches!(path.direction, Direction::Next),
            depth: path.depth as usize,
            min_frequency: path.min_frequency,
            collapse_loops: path.collapse_loops,
            basis: crate::identity::Basis::Session,
            // A path correlates by session and a session does not outlive an
            // identity change, so event time and latest known agree for nearly
            // every row. The field is honoured anyway, because a caller that
            // asked for one and silently got the other would have no way to
            // find out.
            resolution: resolution_of(path.resolution.as_ref()),
        };

        let result = crate::analysis::path(&rows, incomplete, &identity, &question, &self.guards)?;

        let columns = vec![
            "depth".to_string(),
            "node".to_string(),
            "count".to_string(),
            "parent".to_string(),
        ];
        let mut table: Vec<Vec<PropertyValue>> = Vec::new();
        if let Some(root) = &result.root {
            flatten_path(root, None, &mut table);
        }
        let mut warnings = coverage_warnings(&result.coverage, crate::identity::Basis::Session);
        if result.pruned {
            warnings.push(format!(
                "Branches followed by fewer than {} people are not in this tree.",
                path.min_frequency
            ));
        }

        Ok(self.answer(columns, table, result.coverage.incomplete, warnings))
    }

    fn timeline(
        &self,
        timeline: &tallyowl_control_api::types::TimelineQuery,
        request: &QueryRequest,
    ) -> Result<QueryResponse, TallyOwlError> {
        let project_id = to_id(&timeline.project_id)?;
        let (rows, incomplete, identity) = self.rows_and_identity(project_id, &timeline.range)?;
        self.refuse_if_short(incomplete, request)?;

        let question = crate::analysis::TimelineQuestion {
            end_user_id: timeline.end_user_id.clone(),
            session_id: timeline.session_id.clone(),
            kinds: timeline
                .kinds
                .iter()
                .map(control_kind_name)
                .map(str::to_string)
                .collect(),
            limit: timeline.limit as usize,
            after: timeline.cursor.as_deref().and_then(read_cursor),
            // Latest known unless the caller says otherwise: somebody looking
            // at one end user wants everything that turned out to be theirs,
            // including what they did before they signed in.
            resolution: resolution_of(timeline.resolution.as_ref()),
        };

        let result =
            crate::analysis::timeline(&rows, incomplete, &identity, &question, &self.guards)?;
        let next_cursor = result
            .next_after
            .map(|(at, event_id)| write_cursor(at, &event_id));

        let columns = vec![
            "occurred_at".to_string(),
            "kind".to_string(),
            "name".to_string(),
            "event_id".to_string(),
            "session_id".to_string(),
        ];
        let table: Vec<Vec<PropertyValue>> = result
            .rows
            .iter()
            .map(|row| {
                vec![
                    PropertyValue::Integer(row.occurred_at),
                    PropertyValue::Text(row.kind.clone()),
                    PropertyValue::Text(row.name.clone()),
                    PropertyValue::Bytes(row.event_id.to_vec()),
                    row.session_id
                        .clone()
                        .map(PropertyValue::Text)
                        .unwrap_or(PropertyValue::Null),
                ]
            })
            .collect();

        let mut answer = self.answer(columns, table, result.coverage.incomplete, Vec::new());
        answer.metadata.next_cursor = next_cursor;
        Ok(answer)
    }

    /// Which touch earned each conversion, and how much of its value.
    ///
    /// The whole of the rule set is in [`crate::attribution`]. This turns a wire
    /// question into that module's question, runs it, and lays the answer out as
    /// columns. The two things it decides here are the ones that are not the
    /// model's:
    ///
    /// - **the settings come from the project**, never from the request. D40
    ///   makes them configuration, and a caller who could send weights could
    ///   make one campaign outrank another by asking differently;
    /// - **consent comes from the compiled policy**, for the same reason.
    fn attribution(
        &self,
        attribution: &tallyowl_control_api::types::AttributionQuery,
        request: &QueryRequest,
    ) -> Result<QueryResponse, TallyOwlError> {
        let project_id = to_id(&attribution.project_id)?;
        let settings = self.attribution.settings(project_id);
        let question = crate::attribution::Question {
            goal: attribution.conversion_goal.clone(),
            model: crate::attribution::from_wire_model(&attribution.model),
            lookback_ms: if attribution.lookback_ms > 0 {
                attribution.lookback_ms
            } else {
                settings.lookback_ms
            },
            dimension: breakdown_dimension(attribution.breakdown.as_ref()),
            touch_filter: attribution
                .touch_filter
                .as_deref()
                .map(|encoded| expr::prepare_unchecked(encoded, self.max_expression_depth))
                .transpose()?,
            needs_consent: self
                .policy
                .for_project(project_id)
                .attribution_needs_consent,
            resolution: resolution_of(attribution.resolution.as_ref()),
        };
        // The window and the model are checked before the scan, not after it.
        // A refusal that had already read the range has paid the cost it
        // exists to avoid. `docs/QUERY.md` section 14.
        settings.check_model(question.model)?;
        settings.check_window(question.lookback_ms)?;

        let (rows, incomplete, identity) =
            self.rows_and_identity(project_id, &attribution.range)?;
        self.refuse_if_short(incomplete, request)?;

        let answer = crate::attribution::run(&rows, incomplete, &identity, &question, &settings)?;

        let columns = vec![
            question.dimension.as_str().to_string(),
            "credited_value".to_string(),
            "conversions".to_string(),
            "touches".to_string(),
            "assists".to_string(),
        ];
        let table: Vec<Vec<PropertyValue>> = answer
            .rows
            .iter()
            .map(|row| {
                vec![
                    PropertyValue::Text(row.dimension.clone()),
                    PropertyValue::Decimal(row.value.to_text()),
                    PropertyValue::Float(row.conversions),
                    PropertyValue::Unsigned(row.touches),
                    PropertyValue::Unsigned(row.assists),
                ]
            })
            .collect();

        Ok(self.answer(
            columns,
            table,
            answer.coverage.incomplete,
            attribution_warnings(&answer),
        ))
    }

    /// The campaign report: touches, people, conversions, value, cost, return.
    fn campaign_summary(
        &self,
        summary: &tallyowl_control_api::types::CampaignSummaryQuery,
        request: &QueryRequest,
    ) -> Result<QueryResponse, TallyOwlError> {
        use crate::campaign::Dimension;
        use tallyowl_control_api::types::CampaignSummaryQuery_dimension as WireDimension;

        let project_id = to_id(&summary.project_id)?;
        let settings = self.attribution.settings(project_id);
        let question = crate::attribution::Question {
            goal: summary.conversion_goal.clone(),
            model: crate::attribution::from_wire_model(&summary.model),
            lookback_ms: if summary.lookback_ms > 0 {
                summary.lookback_ms
            } else {
                settings.lookback_ms
            },
            dimension: match summary.dimension {
                None | Some(WireDimension::Campaign) => Dimension::Campaign,
                Some(WireDimension::Channel) => Dimension::Channel,
                Some(WireDimension::Source) => Dimension::Source,
                Some(WireDimension::Medium) => Dimension::Medium,
                Some(WireDimension::Content) => Dimension::Content,
            },
            touch_filter: summary
                .touch_filter
                .as_deref()
                .map(|encoded| expr::prepare_unchecked(encoded, self.max_expression_depth))
                .transpose()?,
            needs_consent: self
                .policy
                .for_project(project_id)
                .attribution_needs_consent,
            resolution: resolution_of(summary.resolution.as_ref()),
        };
        settings.check_model(question.model)?;
        settings.check_window(question.lookback_ms)?;

        let (rows, incomplete, identity) = self.rows_and_identity(project_id, &summary.range)?;
        self.refuse_if_short(incomplete, request)?;

        let report =
            crate::attribution::summarize(&rows, incomplete, &identity, &question, &settings)?;

        let columns = vec![
            question.dimension.as_str().to_string(),
            "touches".to_string(),
            "people".to_string(),
            "sessions".to_string(),
            "conversions".to_string(),
            "assists".to_string(),
            "value".to_string(),
            "cost".to_string(),
            "return".to_string(),
        ];
        let table: Vec<Vec<PropertyValue>> = report
            .rows
            .iter()
            .map(|row| {
                vec![
                    PropertyValue::Text(row.dimension.clone()),
                    PropertyValue::Unsigned(row.touches),
                    PropertyValue::Unsigned(row.people),
                    PropertyValue::Unsigned(row.sessions),
                    PropertyValue::Float(row.conversions),
                    // What took part and earned nothing under this model. A
                    // single-touch model hides it, and it is the column that
                    // says which campaigns the model is not paying for.
                    PropertyValue::Unsigned(row.assists),
                    PropertyValue::Decimal(row.value.to_text()),
                    PropertyValue::Decimal(row.cost.to_text()),
                    // No cost imported means no return. A zero here would read
                    // as "this campaign earned nothing for its spend", and a
                    // campaign with no spend recorded earned everything for
                    // nothing.
                    row.return_on_spend
                        .map(PropertyValue::Float)
                        .unwrap_or(PropertyValue::Null),
                ]
            })
            .collect();

        let mut warnings = vec![format!(
            "The credited value uses the `{}` model at version {}, under settings version {}.",
            report.model.as_str(),
            report.model_version,
            report.settings_version
        )];
        if report.unattributed > 0 {
            warnings.push(format!(
                "{} conversions worth {} had no touch inside the window, so no campaign is credited with them.",
                report.unattributed,
                report.unattributed_value.to_text()
            ));
        }
        if question.dimension != Dimension::Campaign {
            warnings.push(
                "An imported cost names a campaign and does not say how that spend divided between the channels it reached, so the cost and return columns are empty for this breakdown."
                    .to_string(),
            );
        }

        Ok(self.answer(columns, table, report.coverage.incomplete, warnings))
    }

    fn execute(
        &self,
        node: &QueryNodeBox,
        request: &QueryRequest,
        started: Instant,
    ) -> Result<Stage, TallyOwlError> {
        // A budget that only bounded the whole query would let one pathological
        // subtree run for the whole of it. The check is at each operator.
        self.check_deadline(started, request)?;

        match node.node {
            QueryNodeKind::Scan => {
                let scan = node.scan.as_ref().ok_or_else(|| missing("a scan"))?;
                self.scan(scan)
            }
            QueryNodeKind::Filter => {
                let filter = node.filter.as_ref().ok_or_else(|| missing("a filter"))?;
                let inner = child(&filter.input)?;

                // A filter straight over a scan is the shape a point lookup
                // takes, and the store can answer it from the locator instead
                // of reading the whole range. See `lookup_scan`.
                let input = match self.lookup_scan(&inner, &filter.filter)? {
                    Some(stage) => stage,
                    None => self.execute(&inner, request, started)?,
                };
                let Stage::Rows {
                    rows,
                    basis,
                    incomplete,
                    zone,
                } = input
                else {
                    return Err(unsupported(
                        "A filter reads from a scan or from another filter in this release. Filtering an aggregate result arrives later.",
                    ));
                };
                let prepared: Prepared =
                    expr::prepare(&filter.filter, &rows, self.max_expression_depth)?;
                Ok(Stage::Rows {
                    rows: rows.into_iter().filter(|row| prepared.keeps(row)).collect(),
                    basis,
                    incomplete,
                    zone,
                })
            }
            QueryNodeKind::Aggregate => {
                let aggregate = node
                    .aggregate
                    .as_ref()
                    .ok_or_else(|| missing("an aggregate"))?;
                // **Ask the tablets to do the arithmetic, when they can.**
                // `docs/QUERY.md` section 5: the coordinator merges partial
                // states and never pulls raw rows to aggregate. It applies to
                // an aggregate directly over a scan, because a filter above the
                // scan is expression work this store contract does not carry.
                if let Some(stage) = self.pushed_down_aggregate(node, aggregate)? {
                    return Ok(stage);
                }
                let input = self.execute(&child(&aggregate.input)?, request, started)?;
                let Stage::Rows {
                    rows,
                    basis,
                    incomplete,
                    zone,
                } = input
                else {
                    return Err(unsupported(
                        "An aggregate reads from a scan or a filter in this release.",
                    ));
                };
                self.aggregate(aggregate, rows, basis, incomplete, zone, None)
            }
            QueryNodeKind::Project => {
                let project = node.project.as_ref().ok_or_else(|| missing("a projection"))?;
                let input = self.execute(&child(&project.input)?, request, started)?;
                self.project(project, input)
            }
            QueryNodeKind::Sort => {
                let sort = node.sort.as_ref().ok_or_else(|| missing("a sort"))?;
                let input = self.execute(&child(&sort.input)?, request, started)?;
                let table = self.materialize(input);
                self.sort(sort, table)
            }
            QueryNodeKind::Limit => {
                let limit = node.limit.as_ref().ok_or_else(|| missing("a limit"))?;
                let input = self.execute(&child(&limit.input)?, request, started)?;
                let table = self.materialize(input);
                Ok(Stage::Table(self.limit(limit, table)?))
            }
            QueryNodeKind::Union => {
                let union: &UnionNode = node.union.as_ref().ok_or_else(|| missing("a union"))?;
                self.union(union, request, started)
            }
            QueryNodeKind::Join => Err(unsupported(
                "This installation does not answer a join yet. Correlating two datasets on an exact ID arrives with the trace query.",
            )),
        }
    }

    fn check_deadline(
        &self,
        started: Instant,
        request: &QueryRequest,
    ) -> Result<(), TallyOwlError> {
        let allowed = request
            .budget
            .as_ref()
            .and_then(|b| b.deadline_ms)
            .filter(|deadline| *deadline > 0)
            .map(|deadline| deadline.min(self.max_runtime_ms))
            .unwrap_or(self.max_runtime_ms);
        if allowed > 0 && started.elapsed().as_millis() as i64 > allowed {
            return Err(TallyOwlError::new(
                tallyowl_obs::ErrorCode::BudgetExceeded,
                format!(
                    "This query ran for longer than the {allowed} milliseconds it was allowed."
                ),
            )
            .retryable(false));
        }
        Ok(())
    }

    fn scan(&self, scan: &ScanNode) -> Result<Stage, TallyOwlError> {
        let kind = check_scan(scan)?;
        let project_id = to_id(&scan.project_id)?;
        let basis = to_basis(&scan.range.basis);
        let scanned = self
            .store
            .scan(
                project_id,
                scan.range.range_start,
                scan.range.range_end,
                basis,
            )
            .map_err(crate::ingest::to_service_error)?;

        // One logical event, whatever the physical rows. A dashboard that
        // reported a spike because a collector retried would be a wrong answer,
        // and deduplicating here means every operator above sees the same rows
        // a count would.
        let mut seen: BTreeSet<[u8; 16]> = BTreeSet::new();
        let rows: Vec<EventRow> = scanned
            .rows
            .into_iter()
            .filter(|row| seen.insert(row.event_id))
            // A dataset is a view over the one stored shape. `events` is every
            // row; the rest select the kind they name. D25 keeps the store
            // contract small, and this is where the views live rather than in
            // the store.
            .filter(|row| kind.is_none_or(|wanted| row.kind == wanted))
            .filter(|row| keeps_derived(scan.scan.clone(), row))
            .collect();

        Ok(Stage::Rows {
            rows,
            basis,
            // A query over data the store could not fully read must say so.
            // Every operator above carries the flag, so an aggregate over a
            // damaged segment is refused rather than reported as a smaller
            // number.
            incomplete: scanned.incomplete,
            zone: Zone::named(scan.range.timezone.as_deref())?,
        })
    }

    /// Answer an aggregate from the tablets' partial states, when it can be.
    ///
    /// `None` means it cannot, and the ordinary path runs. Every reason to say
    /// no is a reason the push-down would change an answer rather than a reason
    /// it would be awkward:
    ///
    /// - **the store has no parts to ask.** A home installation is one node,
    ///   and folding its own rows is what it did before this existed;
    /// - **the input is not a scan.** A filter above the scan is expression
    ///   work, and pushing it down means pushing the expression language into
    ///   the storage contract. D25 keeps that contract small;
    /// - **a measure has no partial state.** A counter rate and a histogram
    ///   quantile both carry a state that is not a number, and neither is
    ///   pushed down in this release. `encode_partial_groups` says so by
    ///   answering `None`, which is the tablet's own refusal travelling back.
    fn pushed_down_aggregate(
        &self,
        node: &QueryNodeBox,
        aggregate: &AggregateNode,
    ) -> Result<Option<Stage>, TallyOwlError> {
        // An aggregate over a scan is pushed down, and so is an aggregate over
        // filters over a scan: the predicate travels inside the plan bytes the
        // cluster contract already carries opaquely, and the tablet evaluates
        // it with the coordinator's own expression code. The owner approved
        // lifting L136's filter exclusion at the Phase 11 review; the store
        // contract itself is unchanged, which is what D25 protects.
        let mut input = child(&aggregate.input)?;
        while input.node == QueryNodeKind::Filter {
            let Some(filter) = input.filter.as_ref() else {
                return Ok(None);
            };
            input = child(&filter.input)?;
        }
        if input.node != QueryNodeKind::Scan {
            return Ok(None);
        }
        let Some(scan) = input.scan.as_ref() else {
            return Ok(None);
        };
        let project_id = to_id(&scan.project_id)?;
        let basis = to_basis(&scan.range.basis);
        let plan = tallyowl_control_api::codec::encode_query_node_box(node);
        let Some(partials) = self
            .store
            .partial_aggregates(
                &plan,
                project_id,
                scan.range.range_start,
                scan.range.range_end,
                basis,
            )
            .map_err(crate::ingest::to_service_error)?
        else {
            return Ok(None);
        };
        let zone = Zone::named(scan.range.timezone.as_deref())?;
        Ok(Some(self.aggregate(
            aggregate,
            Vec::new(),
            basis,
            !partials.complete,
            zone,
            Some(partials.states),
        )?))
    }

    /// One tablet's partial state for an aggregate the coordinator planned.
    ///
    /// **The tablet runs the coordinator's own aggregation.** That is the whole
    /// reason the plan travels as an encoded node rather than as a declaration
    /// of measures on the cluster contract: a second implementation of a sum is
    /// a second answer waiting to happen, and L102 records what that costs.
    pub fn partial_aggregate(&self, plan: &[u8]) -> Result<Option<Vec<u8>>, TallyOwlError> {
        let boxed = tallyowl_control_api::codec::decode_query_node_box(plan).map_err(|e| {
            TallyOwlError::internal(format!("A pushed-down aggregate could not be read: {e}"))
        })?;
        let aggregate = boxed
            .aggregate
            .as_ref()
            .ok_or_else(|| missing("an aggregate"))?;
        // The plan may hold filters between the aggregate and the scan. They
        // are collected on the way down and applied on the way back up, in the
        // order the executor would: the one closest to the scan first. This is
        // the same `expr` code the coordinator runs, which is the point — one
        // implementation of the predicate, wherever it is evaluated.
        let mut filters = Vec::new();
        let mut scan_node = child(&aggregate.input)?;
        while scan_node.node == QueryNodeKind::Filter {
            let filter = scan_node
                .filter
                .as_ref()
                .ok_or_else(|| missing("a filter"))?;
            filters.push(filter.filter.clone());
            scan_node = child(&filter.input)?;
        }
        let scan = scan_node.scan.as_ref().ok_or_else(|| missing("a scan"))?;

        let Stage::Rows {
            mut rows,
            basis,
            zone,
            ..
        } = self.scan(scan)?
        else {
            return Ok(None);
        };
        for filter in filters.iter().rev() {
            let prepared: Prepared = expr::prepare(filter, &rows, self.max_expression_depth)?;
            rows.retain(|row| prepared.keeps(row));
        }

        let bucketing = bucketing(aggregate, zone)?;
        let dimensions: Vec<(String, expr::Field)> = aggregate
            .dimensions
            .iter()
            .map(|dimension| (dimension.alias.clone(), field_of(dimension)))
            .collect();
        let mut groups: BTreeMap<Vec<GroupKey>, Vec<Accumulator>> = BTreeMap::new();
        fold_rows(
            &mut groups,
            &rows,
            basis,
            &bucketing,
            &dimensions,
            &aggregate.measures,
        )?;
        Ok(encode_partial_groups(&groups))
    }

    /// Group by dimensions, and by a time bucket when the query names an
    /// interval.
    fn aggregate(
        &self,
        aggregate: &AggregateNode,
        rows: Vec<EventRow>,
        basis: TimeBasis,
        incomplete: bool,
        zone: Zone,
        // One encoded partial state for each tablet, when the aggregate was
        // pushed down. `None` means the rows above are what to fold.
        partials: Option<Vec<Vec<u8>>>,
    ) -> Result<Stage, TallyOwlError> {
        if aggregate.measures.is_empty() {
            return Err(TallyOwlError::invalid_argument(
                "This query groups rows and asks for nothing about them. Add a measure.",
            ));
        }
        // Every measure is checked before any row is read. Checking lazily, on
        // the first row that reached an accumulator, meant a query over an
        // empty range answered with no rows instead of saying the measure does
        // not exist here — a silent wrong answer, which FAILURE_MODES.md
        // section 2 ranks above a stopped request.
        for measure in &aggregate.measures {
            if let Accumulator::Unsupported(kind) = Accumulator::new(measure) {
                return Err(unsupported_measure(&kind));
            }
            if measure.kind == MeasureKind::Quantile
                && !measure.quantile.is_some_and(|q| (0.0..=1.0).contains(&q))
            {
                return Err(TallyOwlError::invalid_argument(format!(
                    "The measure `{}` asks for a quantile and names none between 0 and 1. Set `quantile` to a number such as 0.99.",
                    measure.alias
                )));
            }
        }
        let bucketing = bucketing(aggregate, zone)?;

        // A dimension is a field reference, and D20's rule about a name holding
        // more than one type applies to it exactly as it does inside a filter.
        let dimensions: Vec<(String, expr::Field)> = aggregate
            .dimensions
            .iter()
            .map(|dimension| (dimension.alias.clone(), field_of(dimension)))
            .collect();
        check_dimension_types(&dimensions, &rows)?;

        let mut columns: Vec<String> = Vec::new();
        if bucketing.is_some() {
            columns.push("bucket".to_string());
        }
        columns.extend(dimensions.iter().map(|(alias, _)| alias.clone()));
        columns.extend(aggregate.measures.iter().map(|m| m.alias.clone()));

        // A group key orders by its values, so the result comes back in a
        // stable order without a sort. A dashboard that redrew in a different
        // order on every refresh would be its own bug report.
        let mut groups: BTreeMap<Vec<GroupKey>, Vec<Accumulator>> = BTreeMap::new();
        match partials {
            // **Every tablet already did the arithmetic.** This is the
            // push-down `docs/QUERY.md` section 5 asks for and L101 recorded as
            // not built: a sum arrives as a sum, and the coordinator adds two
            // numbers instead of moving two ranges of rows.
            Some(states) => {
                for state in &states {
                    merge_partial_groups(&mut groups, state, &aggregate.measures)?;
                }
            }
            None => fold_rows(
                &mut groups,
                &rows,
                basis,
                &bucketing,
                &dimensions,
                &aggregate.measures,
            )?,
        }

        let mut result_rows = Vec::with_capacity(groups.len());
        for (key, accumulators) in groups {
            let mut values: Vec<PropertyValue> = key.iter().map(GroupKey::value).collect();
            for (index, accumulator) in accumulators.iter().enumerate() {
                // A measure whose state cannot produce an answer says so, once
                // the group is whole. `histogram_merge` over two bucket layouts
                // is the case QUERY.md section 7 names.
                if let Some(failure) = accumulator.failure(&aggregate.measures[index]) {
                    return Err(failure);
                }
                values.push(accumulator.finish());
            }
            result_rows.push(values);
        }

        let exactness = aggregate
            .measures
            .iter()
            .map(|measure| Exactness {
                alias: measure.alias.clone(),
                // Every measure this release answers is exact. An exact measure
                // never becomes approximate on its own, so this is a fact
                // rather than a hope.
                exact: true,
                method: None,
                error_bound: None,
            })
            .collect();

        Ok(Stage::Table(Table {
            columns,
            rows: result_rows,
            exactness,
            incomplete,
        }))
    }

    fn project(&self, project: &ProjectNode, input: Stage) -> Result<Stage, TallyOwlError> {
        let Stage::Rows {
            rows, incomplete, ..
        } = input
        else {
            return Err(unsupported(
                "A projection reads from a scan or a filter in this release.",
            ));
        };
        if project.project_fields.is_empty() {
            return Err(TallyOwlError::invalid_argument(
                "This query selects no columns. Name at least one.",
            ));
        }
        let fields: Vec<(String, expr::Field)> = project
            .project_fields
            .iter()
            .map(|dimension| (dimension.alias.clone(), field_of(dimension)))
            .collect();
        check_dimension_types(&fields, &rows)?;

        Ok(Stage::Table(Table {
            columns: fields.iter().map(|(alias, _)| alias.clone()).collect(),
            rows: rows
                .iter()
                .map(|row| {
                    fields
                        .iter()
                        .map(|(_, field)| {
                            field
                                .read(row)
                                .value()
                                .cloned()
                                .unwrap_or(PropertyValue::Null)
                        })
                        .collect()
                })
                .collect(),
            exactness: Vec::new(),
            incomplete,
        }))
    }

    fn sort(&self, sort: &SortNode, mut table: Table) -> Result<Stage, TallyOwlError> {
        use tallyowl_control_api::types::SortKey_direction as Direction;
        if sort.sort.is_empty() {
            return Err(TallyOwlError::invalid_argument(
                "This query asks for a sort and names no key.",
            ));
        }
        let mut keys = Vec::with_capacity(sort.sort.len());
        for key in &sort.sort {
            let index = table
                .columns
                .iter()
                .position(|column| column == &key.alias)
                .ok_or_else(|| {
                    TallyOwlError::invalid_argument(format!(
                        "This query sorts by `{}`, and the rows it sorts have no column of that name. The columns are: {}.",
                        key.alias,
                        table.columns.join(", ")
                    ))
                })?;
            keys.push((index, key.direction == Direction::Desc));
        }

        table.rows.sort_by(|left, right| {
            for (index, descending) in &keys {
                // A value that is not there sorts last in either direction. It
                // is not the smallest value; it is the absence of one, and a
                // dashboard ordering by revenue should not put the rows with no
                // revenue at the top of a descending list.
                let ordering = match (&left[*index], &right[*index]) {
                    (PropertyValue::Null, PropertyValue::Null) => std::cmp::Ordering::Equal,
                    (PropertyValue::Null, _) => std::cmp::Ordering::Greater,
                    (_, PropertyValue::Null) => std::cmp::Ordering::Less,
                    (a, b) => {
                        let natural = expr::compare(a, b).unwrap_or(std::cmp::Ordering::Equal);
                        if *descending {
                            natural.reverse()
                        } else {
                            natural
                        }
                    }
                };
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
            }
            std::cmp::Ordering::Equal
        });
        Ok(Stage::Table(table))
    }

    fn limit(&self, limit: &LimitNode, mut table: Table) -> Result<Table, TallyOwlError> {
        if limit.cursor.is_some() {
            return Err(unsupported(
                "This installation answers a limit with an offset. A cursor arrives with the saved-query work.",
            ));
        }
        let offset = limit.offset.unwrap_or(0) as usize;
        if offset >= table.rows.len() {
            table.rows.clear();
            return Ok(table);
        }
        table.rows.drain(..offset);
        table.rows.truncate(limit.limit as usize);
        Ok(table)
    }

    fn union(
        &self,
        union: &UnionNode,
        request: &QueryRequest,
        started: Instant,
    ) -> Result<Stage, TallyOwlError> {
        if union.union.is_empty() {
            return Err(TallyOwlError::invalid_argument(
                "This query is a union of nothing. Name at least one input.",
            ));
        }
        let mut combined: Option<Table> = None;
        for encoded in &union.union {
            let stage = self.execute(&child(encoded)?, request, started)?;
            let table = self.materialize(stage);
            match combined.as_mut() {
                None => combined = Some(table),
                Some(held) => {
                    // A union combines inputs with the same output schema.
                    // Combining two different shapes would produce a table
                    // whose columns mean different things in different rows.
                    if held.columns != table.columns {
                        return Err(TallyOwlError::invalid_argument(format!(
                            "A union combines results with the same columns. One side has {}, and another has {}.",
                            held.columns.join(", "),
                            table.columns.join(", ")
                        )));
                    }
                    held.rows.extend(table.rows);
                    held.incomplete |= table.incomplete;
                }
            }
        }
        Ok(Stage::Table(combined.expect("at least one input")))
    }

    /// Turn stored rows into the default table a detail view reads.
    /// Answer a filter over a scan from the locator, when the predicate names
    /// one exact value.
    ///
    /// **This is the difference between reading a range and reading a row.** A
    /// point lookup on a unique `request_id` used to materialise every row in
    /// the time range and then throw all but one away, which measured at 1,063
    /// milliseconds over 438,866 events. The locator already knew which two
    /// segments could hold that value; nothing asked it. See L045 and L079.
    ///
    /// Returns `None` when this is not that shape, and the caller reads the
    /// range as before. Nothing here changes which rows match: the full
    /// predicate still runs on whatever comes back, because a locator prunes
    /// and never answers.
    fn lookup_scan(
        &self,
        input: &QueryNodeBox,
        predicate: &[u8],
    ) -> Result<Option<Stage>, TallyOwlError> {
        if input.node != QueryNodeKind::Scan {
            return Ok(None);
        }
        let scan = input.scan.as_ref().ok_or_else(|| missing("a scan"))?;
        let kind = check_scan(scan)?;
        let project_id = to_id(&scan.project_id)?;
        let basis = to_basis(&scan.range.basis);

        // The predicate is prepared without rows: there are none yet, and this
        // only reads its shape. The caller prepares it again against the rows
        // it gets, which is where a type mismatch is still refused.
        let prepared = expr::prepare_unchecked(predicate, self.max_expression_depth)?;
        let Some((field, value)) = prepared.exact_match() else {
            return Ok(None);
        };
        let Some((column, bytes)) = locator_column(&field, &value) else {
            return Ok(None);
        };

        let found = self
            .store
            .lookup_correlated(&column, &bytes)
            .map_err(crate::ingest::to_service_error)?;

        // Everything the range scan applies, applied here. **Tenancy first**:
        // a correlated lookup reads across the whole store, and one project
        // must never see another's rows because it guessed a request ID.
        let mut seen: BTreeSet<[u8; 16]> = BTreeSet::new();
        let rows: Vec<EventRow> = found
            .rows
            .into_iter()
            .filter(|row| row.project_id == project_id)
            .filter(|row| {
                let at = time_of(row, basis);
                at >= scan.range.range_start && at < scan.range.range_end
            })
            .filter(|row| seen.insert(row.event_id))
            .filter(|row| kind.is_none_or(|wanted| row.kind == wanted))
            .filter(|row| keeps_derived(scan.scan.clone(), row))
            .collect();

        Ok(Some(Stage::Rows {
            rows,
            basis,
            incomplete: found.incomplete,
            zone: Zone::named(scan.range.timezone.as_deref())?,
        }))
    }

    /// The refusal a query gets when part of the range could not be read.
    ///
    /// It names the damaged part. FAILURE_MODES.md procedure 6 requires that,
    /// and a refusal that only says "some of it" sends a person looking through
    /// a whole data directory by hand.
    fn incomplete_message(&self) -> String {
        format!(
            "We could not read all of the stored data for this query, so this answer would be smaller than the truth. Ask for a partial result if an incomplete answer is useful.{}",
            self.damage()
        )
    }

    /// What the store says it cannot read, ready to append to a message.
    fn damage(&self) -> String {
        let reasons = self.store.unreadable();
        if reasons.is_empty() {
            return String::new();
        }
        let named = reasons
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<String>>()
            .join("; ");
        let more = if reasons.len() > 3 {
            format!(" and {} more", reasons.len() - 3)
        } else {
            String::new()
        };
        format!(" What could not be read: {named}{more}.")
    }

    fn materialize(&self, stage: Stage) -> Table {
        match stage {
            Stage::Table(table) => table,
            Stage::Rows {
                rows, incomplete, ..
            } => Table {
                columns: vec![
                    "event_id".to_string(),
                    "kind".to_string(),
                    "name".to_string(),
                    "occurred_at".to_string(),
                ],
                rows: rows
                    .iter()
                    .map(|row| {
                        vec![
                            PropertyValue::Bytes(row.event_id.to_vec()),
                            PropertyValue::Text(row.kind.clone()),
                            PropertyValue::Text(row.name.clone()),
                            PropertyValue::Integer(row.occurred_at),
                        ]
                    })
                    .collect(),
                exactness: Vec::new(),
                incomplete,
            },
        }
    }
}

/// Order a trace so a person reads it as a tree.
///
/// A span comes after its parent, and siblings come in start order. A span
/// whose parent is not in the trace is a root: that happens when the parent was
/// dropped, when it is in another project, and when a producer sent a child
/// before its parent. Treating it as a root shows the span rather than hiding
/// it, and a hidden span is the one a person is looking for.
///
/// A cycle cannot loop this, because a span is emitted once and the walk never
/// revisits one.
fn waterfall(rows: &[EventRow]) -> Vec<(usize, &EventRow)> {
    let span_id = |row: &EventRow| -> Option<String> {
        row.properties
            .get("span_id")
            .map(|(value, _)| value.to_display())
    };
    let parent_id = |row: &EventRow| -> Option<String> {
        row.properties
            .get("parent_span_id")
            .map(|(value, _)| value.to_display())
    };

    let present: BTreeSet<String> = rows.iter().filter_map(span_id).collect();
    let mut children: BTreeMap<String, Vec<&EventRow>> = BTreeMap::new();
    let mut roots: Vec<&EventRow> = Vec::new();
    for row in rows {
        match parent_id(row) {
            Some(parent) if present.contains(&parent) => {
                children.entry(parent).or_default().push(row)
            }
            _ => roots.push(row),
        }
    }
    for list in children.values_mut() {
        list.sort_by_key(|row| row.occurred_at);
    }
    roots.sort_by_key(|row| row.occurred_at);

    let mut out: Vec<(usize, &EventRow)> = Vec::with_capacity(rows.len());
    let mut stack: Vec<(usize, &EventRow)> = roots.into_iter().rev().map(|row| (0, row)).collect();
    let mut emitted: BTreeSet<[u8; 16]> = BTreeSet::new();
    while let Some((depth, row)) = stack.pop() {
        if !emitted.insert(row.event_id) {
            continue;
        }
        out.push((depth, row));
        if let Some(id) = span_id(row) {
            if let Some(list) = children.get(&id) {
                for child in list.iter().rev() {
                    stack.push((depth + 1, child));
                }
            }
        }
    }
    // A span that no walk reached, because its parent chain was itself
    // unreachable. Showing it is better than losing it.
    for row in rows {
        if emitted.insert(row.event_id) {
            out.push((0, row));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Measures
// ---------------------------------------------------------------------------

/// A partial state for one measure over one group.
///
/// QUERY.md section 7 gives the state for each. A count is an integer, a sum is
/// a typed sum, and `count_distinct` is a value set under a cap.
enum Accumulator {
    Count(u64),
    /// A sum that stays exact while every value it saw was exact, and becomes a
    /// float only when a float arrived.
    Sum {
        exact: Option<(i128, u32)>,
        inexact: f64,
        saw_float: bool,
        saw_any: bool,
    },
    Extreme {
        largest: bool,
        held: Option<PropertyValue>,
    },
    Average {
        total: f64,
        count: u64,
    },
    Distinct(BTreeSet<GroupKey>),
    /// Samples of every counter series in this group, kept apart by series, so
    /// `rate` and `increase` never read two series as one that jumped.
    ///
    /// QUERY.md section 7 calls the partial state "per-series values and reset
    /// marks", and section 12.8 says a reset is a decrease in a cumulative
    /// series. Both are here.
    Counter {
        /// True for `rate`, which divides the increase by the period.
        per_second: bool,
        series: BTreeMap<String, Vec<CounterSample>>,
    },
    /// Aligned bucket counts. QUERY.md section 7: exact when the buckets align,
    /// and a typed failure when they do not. TallyOwl never silently rebuckets.
    Histogram {
        /// The quantile to read out of the merged buckets, for `quantile`.
        /// `histogram_merge` reads none and returns the merged shape.
        quantile: Option<f64>,
        merged: Option<MergedHistogram>,
        /// The two layouts that would not align, so the refusal can name both.
        misaligned: Option<(String, String)>,
    },
    /// A measure this release does not answer. It is refused when the first row
    /// reaches it, so the refusal names the measure rather than the query.
    Unsupported(MeasureKind),
}

/// One reading of one counter series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CounterSample {
    /// The end of the period this reading covers, which orders the samples.
    at: i64,
    /// The start the producer reported. A change in it is a restart, and that
    /// is what separates a restart from a counter that ran backwards.
    start_at: i64,
    value: f64,
    /// True when the producer reported a delta rather than a running total.
    delta: bool,
}

/// Bucket counts that every merged point agreed on.
#[derive(Debug, Clone, PartialEq)]
pub struct MergedHistogram {
    bounds: Vec<f64>,
    /// Cumulative: `counts[i]` is every observation at or below `bounds[i]`.
    counts: Vec<u64>,
    count: u64,
    sum: f64,
    /// The canonical bound list, so the next point is compared by equality.
    layout: String,
}

impl Accumulator {
    fn new(measure: &Measure) -> Accumulator {
        match measure.kind {
            MeasureKind::Count => Accumulator::Count(0),
            MeasureKind::Sum => Accumulator::Sum {
                exact: Some((0, 0)),
                inexact: 0.0,
                saw_float: false,
                saw_any: false,
            },
            MeasureKind::Min => Accumulator::Extreme {
                largest: false,
                held: None,
            },
            MeasureKind::Max => Accumulator::Extreme {
                largest: true,
                held: None,
            },
            MeasureKind::Avg => Accumulator::Average {
                total: 0.0,
                count: 0,
            },
            MeasureKind::CountDistinct => Accumulator::Distinct(BTreeSet::new()),
            MeasureKind::Rate => Accumulator::Counter {
                per_second: true,
                series: BTreeMap::new(),
            },
            MeasureKind::Increase => Accumulator::Counter {
                per_second: false,
                series: BTreeMap::new(),
            },
            MeasureKind::HistogramMerge => Accumulator::Histogram {
                quantile: None,
                merged: None,
                misaligned: None,
            },
            MeasureKind::Quantile => Accumulator::Histogram {
                // QUERY.md section 12.8: a quantile applies to a histogram
                // through `histogram_merge`. A missing quantile is refused when
                // the measure is checked, before any row is read.
                quantile: Some(measure.quantile.unwrap_or(f64::NAN)),
                merged: None,
                misaligned: None,
            },
            ref other => Accumulator::Unsupported(other.clone()),
        }
    }

    fn add(&mut self, measure: &Measure, row: &EventRow) -> Result<(), TallyOwlError> {
        if let Accumulator::Unsupported(kind) = self {
            return Err(unsupported_measure(kind));
        }
        if let Accumulator::Count(count) = self {
            *count += 1;
            return Ok(());
        }
        // A metric measure reads the metric point's own columns rather than a
        // field the caller names, because the columns it needs are four and a
        // measure names one.
        if let Accumulator::Counter { series, .. } = self {
            return add_counter_sample(measure, row, series);
        }
        if let Accumulator::Histogram {
            merged, misaligned, ..
        } = self
        {
            return add_histogram(measure, row, merged, misaligned);
        }
        let field = measure.field.as_ref().ok_or_else(|| {
            TallyOwlError::invalid_argument(format!(
                "The measure `{}` needs a field to read, and this query names none.",
                measure.alias
            ))
        })?;
        let read = expr::Field {
            name: field.name.clone(),
            value_type: field.value_type.clone(),
            origin: field.origin.clone(),
        };
        let Some(value) = read.read(row).value().cloned() else {
            // A row with no value for this field contributes nothing. It does
            // not contribute a zero, which would pull an average down.
            return Ok(());
        };

        match self {
            Accumulator::Count(_)
            | Accumulator::Unsupported(_)
            | Accumulator::Counter { .. }
            | Accumulator::Histogram { .. } => unreachable!(),
            Accumulator::Sum {
                exact,
                inexact,
                saw_float,
                saw_any,
            } => {
                *saw_any = true;
                match &value {
                    PropertyValue::Float(number) => {
                        *saw_float = true;
                        *inexact += number;
                    }
                    other => match exact_parts(other) {
                        Some(parts) => {
                            *inexact += expr::as_float(other).unwrap_or(0.0);
                            if let Some(held) = exact.as_mut() {
                                match add_exact(*held, parts) {
                                    Some(sum) => *held = sum,
                                    // The exact sum overflowed. Say so by
                                    // dropping to the float rather than
                                    // reporting a wrapped total.
                                    None => *exact = None,
                                }
                            }
                        }
                        None => {
                            return Err(TallyOwlError::invalid_argument(format!(
                                "The measure `{}` sums a value that is not a number.",
                                measure.alias
                            )))
                        }
                    },
                }
            }
            Accumulator::Extreme { largest, held } => {
                let replace = match held {
                    None => true,
                    Some(current) => match expr::compare(&value, current) {
                        Some(std::cmp::Ordering::Greater) => *largest,
                        Some(std::cmp::Ordering::Less) => !*largest,
                        _ => false,
                    },
                };
                if replace {
                    *held = Some(value);
                }
            }
            Accumulator::Average { total, count } => {
                let Some(number) = expr::as_float(&value) else {
                    return Err(TallyOwlError::invalid_argument(format!(
                        "The measure `{}` averages a value that is not a number.",
                        measure.alias
                    )));
                };
                *total += number;
                *count += 1;
            }
            Accumulator::Distinct(seen) => {
                let key = GroupKey::of(Some(&value));
                if seen.len() >= DISTINCT_CAP && !seen.contains(&key) {
                    // An exact measure that reaches its cap fails and names the
                    // approximate one. It never becomes approximate on its own.
                    return Err(TallyOwlError::new(
                        tallyowl_obs::ErrorCode::BudgetExceeded,
                        format!(
                            "The measure `{}` counts distinct values exactly, and this query passed {DISTINCT_CAP} of them. Ask for `count_distinct_approx`, which answers at any size and states its error bound.",
                            measure.alias
                        ),
                    )
                    .retryable(false));
                }
                seen.insert(key);
            }
        }
        Ok(())
    }

    fn finish(&self) -> PropertyValue {
        match self {
            Accumulator::Count(count) => PropertyValue::Unsigned(*count),
            Accumulator::Sum {
                exact,
                inexact,
                saw_float,
                saw_any,
            } => {
                if !*saw_any {
                    return PropertyValue::Null;
                }
                match (saw_float, exact) {
                    // Every value was exact, so the total is exact. Money adds
                    // up to what a customer's own records say.
                    (false, Some((mantissa, scale))) => {
                        PropertyValue::Decimal(expr::render(*mantissa, *scale))
                    }
                    _ => PropertyValue::Float(*inexact),
                }
            }
            Accumulator::Extreme { held, .. } => held.clone().unwrap_or(PropertyValue::Null),
            Accumulator::Average { total, count } => {
                if *count == 0 {
                    PropertyValue::Null
                } else {
                    PropertyValue::Float(total / *count as f64)
                }
            }
            Accumulator::Distinct(seen) => PropertyValue::Unsigned(seen.len() as u64),
            Accumulator::Counter { per_second, series } => finish_counter(*per_second, series),
            Accumulator::Histogram {
                quantile, merged, ..
            } => match (merged, quantile) {
                (None, _) => PropertyValue::Null,
                (Some(histogram), None) => PropertyValue::Text(render_histogram(histogram)),
                (Some(histogram), Some(quantile)) => match quantile_of(histogram, *quantile) {
                    Some(value) => PropertyValue::Float(value),
                    None => PropertyValue::Null,
                },
            },
            Accumulator::Unsupported(_) => PropertyValue::Null,
        }
    }

    /// A measure whose state cannot produce an answer, and why.
    ///
    /// `histogram_merge` is the case QUERY.md section 7 names: it "fails when
    /// bucket boundaries do not align. The caller must request an explicit
    /// rebucket. TallyOwl does not silently rebucket." So the failure arrives
    /// here rather than as a merged shape nothing observed.
    fn failure(&self, measure: &Measure) -> Option<TallyOwlError> {
        let Accumulator::Histogram {
            misaligned: Some((held, arrived)),
            ..
        } = self
        else {
            return None;
        };
        Some(
            TallyOwlError::new(
                tallyowl_obs::ErrorCode::FailedPrecondition,
                format!(
                    "The measure `{}` merges histograms, and this group holds two bucket layouts: [{held}] and [{arrived}]. TallyOwl does not rebucket a histogram on its own, because the merged shape would be one nothing observed. Filter to one layout, or ask the producers to agree on their buckets.",
                    measure.alias
                ),
            )
            .retryable(false),
        )
    }
}

/// Read one metric point into the counter state.
fn add_counter_sample(
    measure: &Measure,
    row: &EventRow,
    series: &mut BTreeMap<String, Vec<CounterSample>>,
) -> Result<(), TallyOwlError> {
    let Some(PropertyValue::Float(value)) = number_property(row, "value") else {
        // A row with no value contributes nothing. A row that is not a metric
        // point has none, so a `rate` over a mixed dataset answers about the
        // metric points in it rather than failing.
        return Ok(());
    };
    let key = text_property(row, "series_key").unwrap_or_else(|| row.name.clone());
    let delta = text_property(row, "temporality").as_deref() == Some("delta");
    let sample = CounterSample {
        at: integer_property(row, "end_at").unwrap_or(row.occurred_at),
        start_at: integer_property(row, "start_at").unwrap_or(row.occurred_at),
        value,
        delta,
    };
    let held = series.entry(key).or_default();
    if held.len() >= COUNTER_SAMPLE_CAP {
        return Err(TallyOwlError::new(
            tallyowl_obs::ErrorCode::BudgetExceeded,
            format!(
                "The measure `{}` reads a counter sample by sample, and this query passed {COUNTER_SAMPLE_CAP} samples of one series. Narrow the time range, or group by a longer interval.",
                measure.alias
            ),
        )
        .retryable(false));
    }
    held.push(sample);
    Ok(())
}

/// The increase of every series in this group, added together.
fn finish_counter(
    per_second: bool,
    series: &BTreeMap<String, Vec<CounterSample>>,
) -> PropertyValue {
    let mut total = 0.0;
    let mut earliest = i64::MAX;
    let mut latest = i64::MIN;
    let mut saw_any = false;

    for samples in series.values() {
        if samples.is_empty() {
            continue;
        }
        saw_any = true;
        let mut ordered = samples.clone();
        ordered.sort_by_key(|sample| sample.at);
        earliest = earliest.min(ordered.first().expect("not empty").at);
        latest = latest.max(ordered.last().expect("not empty").at);

        let mut previous: Option<CounterSample> = None;
        for sample in ordered {
            if sample.delta {
                // A delta already says what happened in its own period, so the
                // periods add and there is no reset to find.
                total += sample.value;
                previous = Some(sample);
                continue;
            }
            match previous {
                // The first cumulative reading of a series is a level rather
                // than an increase. Counting it would report everything the
                // counter ever saw as having happened inside this range.
                None => {}
                Some(before) => {
                    if sample.start_at != before.start_at || sample.value < before.value {
                        // A restart. The producer began again, so everything it
                        // has counted since is the increase. QUERY.md section
                        // 12.7 defines a reset as a decrease in a cumulative
                        // series, and a new start says the same thing earlier.
                        total += sample.value;
                    } else {
                        total += sample.value - before.value;
                    }
                }
            }
            previous = Some(sample);
        }
    }

    if !saw_any {
        return PropertyValue::Null;
    }
    if !per_second {
        return PropertyValue::Float(total);
    }
    let span_ms = latest - earliest;
    if span_ms <= 0 {
        // One reading gives no period to divide by. A rate from a single sample
        // would be an invented number.
        return PropertyValue::Null;
    }
    PropertyValue::Float(total / (span_ms as f64 / 1000.0))
}

/// Merge one stored histogram into the accumulator, or mark the layouts that
/// would not align.
fn add_histogram(
    _measure: &Measure,
    row: &EventRow,
    merged: &mut Option<MergedHistogram>,
    misaligned: &mut Option<(String, String)>,
) -> Result<(), TallyOwlError> {
    let Some(layout) = text_property(row, "histogram_bounds") else {
        return Ok(());
    };
    let Some(counts_text) = text_property(row, "histogram_counts") else {
        return Ok(());
    };
    let bounds = parse_floats(&layout);
    let counts = parse_unsigned(&counts_text);
    if bounds.len() != counts.len() {
        return Ok(());
    }
    let count = match number_property(row, "histogram_count") {
        Some(PropertyValue::Float(value)) => value as u64,
        _ => *counts.last().unwrap_or(&0),
    };
    let sum = match number_property(row, "histogram_sum") {
        Some(PropertyValue::Float(value)) => value,
        _ => 0.0,
    };

    match merged {
        None => {
            *merged = Some(MergedHistogram {
                bounds,
                counts,
                count,
                sum,
                layout,
            })
        }
        Some(held) => {
            if held.layout != layout {
                // Not merged, and not rebucketed. The failure is reported when
                // the group finishes, with both layouts named.
                if misaligned.is_none() {
                    *misaligned = Some((held.layout.clone(), layout));
                }
                return Ok(());
            }
            for (index, value) in counts.iter().enumerate() {
                held.counts[index] = held.counts[index].saturating_add(*value);
            }
            held.count = held.count.saturating_add(count);
            held.sum += sum;
        }
    }
    Ok(())
}

/// The merged shape, in the same canonical form the row carried.
fn render_histogram(histogram: &MergedHistogram) -> String {
    format!(
        "bounds=[{}] counts=[{}] count={} sum={}",
        histogram.layout,
        histogram
            .counts
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<String>>()
            .join(","),
        histogram.count,
        crate::project::format_float(histogram.sum)
    )
}

/// Read a quantile out of merged buckets.
///
/// The value is interpolated inside the bucket the quantile falls in, which is
/// the only thing a bucketed histogram can say. A quantile above the last bound
/// returns that bound, because nothing in the data says how far past it the
/// observations reached.
fn quantile_of(histogram: &MergedHistogram, quantile: f64) -> Option<f64> {
    if !(0.0..=1.0).contains(&quantile) || histogram.count == 0 {
        return None;
    }
    let wanted = quantile * histogram.count as f64;
    let mut lower_bound = 0.0;
    let mut lower_count = 0.0;
    for (index, bound) in histogram.bounds.iter().enumerate() {
        let cumulative = histogram.counts[index] as f64;
        if cumulative >= wanted {
            let inside = cumulative - lower_count;
            if inside <= 0.0 {
                return Some(*bound);
            }
            let position = (wanted - lower_count) / inside;
            return Some(lower_bound + (bound - lower_bound) * position);
        }
        lower_bound = *bound;
        lower_count = cumulative;
    }
    // Past the last bound. The observations are in the unbounded bucket and
    // nothing says how far past it they reached.
    histogram.bounds.last().copied()
}

fn text_property(row: &EventRow, key: &str) -> Option<String> {
    match row.properties.get(key) {
        Some((PropertyValue::Text(text), _)) => Some(text.clone()),
        _ => None,
    }
}

fn integer_property(row: &EventRow, key: &str) -> Option<i64> {
    match row.properties.get(key) {
        Some((PropertyValue::Integer(value), _)) => Some(*value),
        Some((PropertyValue::Unsigned(value), _)) => Some(*value as i64),
        _ => None,
    }
}

/// A number, whatever numeric type it was stored as, as a float.
fn number_property(row: &EventRow, key: &str) -> Option<PropertyValue> {
    let (value, _) = row.properties.get(key)?;
    expr::as_float(value).map(PropertyValue::Float)
}

fn parse_floats(text: &str) -> Vec<f64> {
    text.split(',')
        .filter(|part| !part.is_empty())
        .map(|part| match part {
            "+Inf" => f64::INFINITY,
            "-Inf" => f64::NEG_INFINITY,
            other => other.parse().unwrap_or(f64::NAN),
        })
        .collect()
}

fn parse_unsigned(text: &str) -> Vec<u64> {
    text.split(',')
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// How many samples of one counter series one group holds before the query is
/// refused. A range that needs more is a range a caller should bucket.
const COUNTER_SAMPLE_CAP: usize = 100_000;

fn exact_parts(value: &PropertyValue) -> Option<(i128, u32)> {
    match value {
        PropertyValue::Integer(v) => Some((*v as i128, 0)),
        PropertyValue::Unsigned(v) => Some((*v as i128, 0)),
        PropertyValue::Decimal(_) => decimal_parts(value),
        _ => None,
    }
}

fn decimal_parts(value: &PropertyValue) -> Option<(i128, u32)> {
    let PropertyValue::Decimal(text) = value else {
        return None;
    };
    let text = text.trim();
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = rest.split_once('.').unwrap_or((rest, ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    let mut mantissa: i128 = 0;
    for character in whole.chars().chain(fraction.chars()) {
        let digit = character.to_digit(10)? as i128;
        mantissa = mantissa.checked_mul(10)?.checked_add(digit)?;
    }
    Some((
        if negative { -mantissa } else { mantissa },
        fraction.len() as u32,
    ))
}

fn add_exact(left: (i128, u32), right: (i128, u32)) -> Option<(i128, u32)> {
    let scale = left.1.max(right.1);
    let lift = |value: (i128, u32)| -> Option<i128> {
        10i128
            .checked_pow(scale - value.1)
            .and_then(|factor| value.0.checked_mul(factor))
    };
    Some((lift(left)?.checked_add(lift(right)?)?, scale))
}

/// A group key that orders, so a result comes back in a stable order.
///
/// A float is not a key: two floats that print the same can differ, and a
/// grouping that split on that would be impossible to explain. A float
/// dimension therefore groups by its exact text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum GroupKey {
    Absent,
    Boolean(bool),
    Integer(i64),
    Text(String),
    Bytes(Vec<u8>),
}

impl GroupKey {
    fn of(value: Option<&PropertyValue>) -> GroupKey {
        match value {
            None | Some(PropertyValue::Null) => GroupKey::Absent,
            Some(PropertyValue::Boolean(v)) => GroupKey::Boolean(*v),
            Some(PropertyValue::Integer(v)) => GroupKey::Integer(*v),
            Some(PropertyValue::Unsigned(v)) => GroupKey::Integer(*v as i64),
            Some(PropertyValue::Text(v)) => GroupKey::Text(v.clone()),
            Some(PropertyValue::Bytes(v)) => GroupKey::Bytes(v.clone()),
            Some(PropertyValue::Float(v)) => GroupKey::Text(v.to_string()),
            Some(PropertyValue::Decimal(v)) => GroupKey::Text(v.clone()),
        }
    }

    fn value(&self) -> PropertyValue {
        match self {
            GroupKey::Absent => PropertyValue::Null,
            GroupKey::Boolean(v) => PropertyValue::Boolean(*v),
            GroupKey::Integer(v) => PropertyValue::Integer(*v),
            GroupKey::Text(v) => PropertyValue::Text(v.clone()),
            GroupKey::Bytes(v) => PropertyValue::Bytes(v.clone()),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn field_of(dimension: &Dimension) -> expr::Field {
    expr::Field {
        name: dimension.field.name.clone(),
        value_type: dimension.field.value_type.clone(),
        origin: dimension.field.origin.clone(),
    }
}

/// D20 again: a dimension that matches several types without a selection would
/// split one group into two and look like a real difference.
fn check_dimension_types(
    dimensions: &[(String, expr::Field)],
    rows: &[EventRow],
) -> Result<(), TallyOwlError> {
    for (alias, field) in dimensions {
        if field.value_type.is_some() || expr::BUILT_IN.contains(&field.name.as_str()) {
            continue;
        }
        let mut kinds: BTreeSet<&'static str> = BTreeSet::new();
        for row in rows {
            if let Some((value, _)) = row.properties.get(&field.name) {
                kinds.insert(value.type_name());
            }
        }
        if kinds.len() > 1 {
            let named: Vec<&str> = kinds.into_iter().collect();
            return Err(TallyOwlError::new(
                tallyowl_obs::ErrorCode::InvalidArgument,
                format!(
                    "The dimension `{alias}` reads `{}`, which holds more than one type here: {}. Say which one this query means, or the same value would appear as two groups.",
                    field.name,
                    named.join(", ")
                ),
            )
            .retryable(false));
        }
    }
    Ok(())
}

fn time_of(row: &EventRow, basis: TimeBasis) -> i64 {
    match basis {
        TimeBasis::OccurredAt => row.occurred_at,
        TimeBasis::ReceivedAt => row.received_at,
        TimeBasis::CommittedAt => row.committed_at,
    }
}

fn to_wire(value: &PropertyValue) -> tallyowl_control_api::types::TypedValue {
    use tallyowl_wire::{control as wire, Value};
    wire::write(&match value {
        PropertyValue::Null => Value::Null,
        PropertyValue::Boolean(v) => Value::Boolean(*v),
        PropertyValue::Integer(v) => Value::Integer(*v),
        PropertyValue::Unsigned(v) => Value::Unsigned(*v),
        PropertyValue::Float(v) => Value::Float(*v),
        PropertyValue::Decimal(text) => {
            Value::decimal_from_text(text).unwrap_or_else(|| Value::Text(text.clone()))
        }
        PropertyValue::Text(v) => Value::Text(v.clone()),
        PropertyValue::Bytes(v) => Value::Bytes(v.clone()),
    })
}

/// Decode one child node. The size bound is in the specification; this is where
/// a malformed child becomes a message a caller can act on rather than a
/// decoder failure they cannot.
fn child(encoded: &[u8]) -> Result<QueryNodeBox, TallyOwlError> {
    decode_query_node_box(encoded).map_err(|e| {
        TallyOwlError::invalid_argument(format!("Part of this query could not be read. {e}"))
    })
}

/// A node whose `node` field names a part the message did not carry. The
/// discriminant and the field always travel together, so this is a malformed
/// request rather than a case the executor should guess at.
fn missing(named: &str) -> TallyOwlError {
    TallyOwlError::invalid_argument(format!(
        "Part of this query says it is {named} and carries no {named}. Send the whole query."
    ))
}

/// A scan always has a time range. A scan without one is an error, and an
/// unbounded scan is the query that reads the whole retention window.
///
/// Returns the telemetry kind the dataset selects, or `None` for a dataset that
/// reads every kind.
fn check_scan(scan: &ScanNode) -> Result<Option<&'static str>, TallyOwlError> {
    if scan.range.range_end <= scan.range.range_start {
        return Err(TallyOwlError::invalid_argument(
            "This query asks for a time range that ends before it starts. Set the end after the start.",
        ));
    }
    match scan.scan {
        // `events` is every kind a **producer** sent. It is not every row: a
        // derived rollup is TallyOwl's own output, and counting it in a trend
        // over "events" made a chart report work nobody did. The reference
        // application's ledger had to filter them out by hand, which is the
        // sign that the dataset did not mean what its name says. See L070.
        Dataset::Events => Ok(None),
        // An occurrence and a group read the same rows. A group is what an
        // aggregate over `error_group` produces, so the difference is in the
        // query rather than in the dataset, and both names resolve here so a
        // caller can write the one that says what they meant.
        Dataset::ErrorOccurrences | Dataset::ErrorGroups => Ok(Some("error")),
        Dataset::Spans => Ok(Some("span")),
        Dataset::MetricPoints => Ok(Some("metric-point")),
        Dataset::Conversions => Ok(Some("conversion")),
        Dataset::CampaignTouches => Ok(Some("campaign-touch")),
        Dataset::CampaignCosts => Ok(Some("campaign-cost")),
        Dataset::IdentityEdges => Err(unsupported(
            "This installation does not answer a query over identity edges yet. Identity and aliasing arrive with the product behaviour work.",
        )),
    }
}

/// How a time bucket is found for one row.
///
/// **A fixed interval and a calendar interval are two different questions.** A
/// fixed one is arithmetic on an instant and a timezone cannot change it. A
/// calendar one is a question about a wall clock: the day that starts at local
/// midnight is 23 hours long on one Sunday each spring, and a month is 28, 29,
/// 30, or 31 days. L134.
enum Bucketing {
    Fixed(i64),
    Calendar(calendar::Unit, Zone),
}

impl Bucketing {
    fn start_of(&self, at: i64) -> i64 {
        match self {
            Bucketing::Fixed(span) => at - at.rem_euclid(*span),
            Bucketing::Calendar(unit, zone) => zone.start_of(at, *unit),
        }
    }
}

fn bucketing(aggregate: &AggregateNode, zone: Zone) -> Result<Option<Bucketing>, TallyOwlError> {
    let Some(interval) = aggregate.interval.as_ref() else {
        return Ok(None);
    };
    if let Some(fixed) = interval.fixed_ms {
        if fixed <= 0 {
            return Err(TallyOwlError::invalid_argument(
                "This query asks to count by a time bucket that has no length. Set an interval longer than nothing.",
            ));
        }
        return Ok(Some(Bucketing::Fixed(fixed)));
    }
    let Some(calendar) = interval.calendar.as_ref() else {
        return Ok(None);
    };
    use tallyowl_control_api::types::Interval_calendar as Calendar;
    let unit = match calendar {
        Calendar::Hour => calendar::Unit::Hour,
        Calendar::Day => calendar::Unit::Day,
        Calendar::Week => calendar::Unit::Week,
        Calendar::Month => calendar::Unit::Month,
    };
    Ok(Some(Bucketing::Calendar(unit, zone)))
}

fn unsupported_measure(kind: &MeasureKind) -> TallyOwlError {
    let named = match kind {
        MeasureKind::CountDistinctApprox => "count_distinct_approx",
        MeasureKind::Quantile => "quantile",
        MeasureKind::QuantileApprox => "quantile_approx",
        MeasureKind::HistogramMerge => "histogram_merge",
        MeasureKind::TopK => "top_k",
        MeasureKind::TopKApprox => "top_k_approx",
        MeasureKind::Rate => "rate",
        MeasureKind::Increase => "increase",
        _ => "that measure",
    };
    unsupported(&format!(
        "This installation answers count, sum, min, max, avg, and count_distinct. `{named}` arrives in a later release."
    ))
}

fn to_basis(basis: &WireBasis) -> TimeBasis {
    match basis {
        WireBasis::OccurredAt => TimeBasis::OccurredAt,
        WireBasis::ReceivedAt => TimeBasis::ReceivedAt,
        WireBasis::CommittedAt => TimeBasis::CommittedAt,
    }
}

fn to_id(bytes: &[u8]) -> Result<[u8; 16], TallyOwlError> {
    bytes.try_into().map_err(|_| {
        TallyOwlError::invalid_argument(
            "This query names a project we do not recognise. A project ID is 16 bytes.",
        )
    })
}

fn unsupported(message: &str) -> TallyOwlError {
    TallyOwlError::new(tallyowl_obs::ErrorCode::InvalidArgument, message).retryable(false)
}

/// Every project this request reads.
///
/// A query is a tree, so the projects are wherever the scans are, and a union
/// can hold several. Authorization runs over all of them: checking only the
/// first would let a union read one project a caller may see and one they may
/// not.
pub fn projects_named(request: &QueryRequest) -> Result<Vec<[u8; 16]>, TallyOwlError> {
    let mut out = Vec::new();
    if let Some(trace) = request.trace.as_ref() {
        out.push(to_id(&trace.project_id)?);
    }
    // Every domain operator names its own project, and each one has to be here.
    // **A form this function forgets is a form nothing authorizes**, so a
    // caller who guessed a project identifier would read another tenant's data
    // through it. The match below is exhaustive rather than a list of the forms
    // that happened to exist when it was written, so adding a form to
    // `QueryForm` and not to this stops compiling.
    match request.form {
        QueryForm::Node | QueryForm::Trace => {}
        QueryForm::Funnel => {
            if let Some(funnel) = request.funnel.as_ref() {
                out.push(to_id(&funnel.project_id)?);
            }
        }
        QueryForm::Retention => {
            if let Some(retention) = request.retention.as_ref() {
                out.push(to_id(&retention.project_id)?);
            }
        }
        QueryForm::Path => {
            if let Some(path) = request.path.as_ref() {
                out.push(to_id(&path.project_id)?);
            }
        }
        QueryForm::Timeline => {
            if let Some(timeline) = request.timeline.as_ref() {
                out.push(to_id(&timeline.project_id)?);
            }
        }
        QueryForm::Attribution => {
            if let Some(attribution) = request.attribution.as_ref() {
                out.push(to_id(&attribution.project_id)?);
            }
        }
        QueryForm::CampaignSummary => {
            if let Some(summary) = request.campaign_summary.as_ref() {
                out.push(to_id(&summary.project_id)?);
            }
        }
    }
    if let Some(encoded) = request.node.as_deref() {
        collect_projects(&child(encoded)?, &mut out, 0)?;
    }
    if out.is_empty() {
        // A request that names no project at all cannot be authorized, and
        // running it would mean running it against nothing or against
        // everything. Both are worse than a refusal.
        return Err(TallyOwlError::invalid_argument(
            "This query names no project. Every query names the project whose telemetry it reads.",
        ));
    }
    Ok(out)
}

/// The nesting a query tree may reach before authorization refuses to walk it.
///
/// A tree this deep is refused by the executor as well. The bound is here too
/// because authorization runs first, and a check that could be made to run for
/// ever by an unauthenticated caller is worse than no check.
const MAX_TREE_DEPTH: u32 = 64;

fn collect_projects(
    node: &QueryNodeBox,
    out: &mut Vec<[u8; 16]>,
    depth: u32,
) -> Result<(), TallyOwlError> {
    if depth > MAX_TREE_DEPTH {
        return Err(TallyOwlError::new(
            tallyowl_obs::ErrorCode::BudgetExceeded,
            format!("This query nests operators more than {MAX_TREE_DEPTH} deep."),
        )
        .retryable(false));
    }
    let walk = |encoded: &[u8], out: &mut Vec<[u8; 16]>| -> Result<(), TallyOwlError> {
        collect_projects(&child(encoded)?, out, depth + 1)
    };
    match node.node {
        QueryNodeKind::Scan => {
            if let Some(scan) = node.scan.as_ref() {
                out.push(to_id(&scan.project_id)?);
            }
        }
        QueryNodeKind::Filter => {
            if let Some(filter) = node.filter.as_ref() {
                walk(&filter.input, out)?;
            }
        }
        QueryNodeKind::Project => {
            if let Some(project) = node.project.as_ref() {
                walk(&project.input, out)?;
            }
        }
        QueryNodeKind::Aggregate => {
            if let Some(aggregate) = node.aggregate.as_ref() {
                walk(&aggregate.input, out)?;
            }
        }
        QueryNodeKind::Sort => {
            if let Some(sort) = node.sort.as_ref() {
                walk(&sort.input, out)?;
            }
        }
        QueryNodeKind::Limit => {
            if let Some(limit) = node.limit.as_ref() {
                walk(&limit.input, out)?;
            }
        }
        QueryNodeKind::Join => {
            if let Some(join) = node.join.as_ref() {
                walk(&join.left, out)?;
                walk(&join.right, out)?;
            }
        }
        QueryNodeKind::Union => {
            if let Some(union) = node.union.as_ref() {
                for input in &union.union {
                    walk(input, out)?;
                }
            }
        }
    }
    Ok(())
}

/// Whether a caller asked for a reading that this head can promise.
pub fn check_consistency(request: &QueryRequest) -> Result<(), TallyOwlError> {
    match request.consistency {
        // A home installation has one voter, so a committed read is the only
        // reading there is, and a bounded-stale request gets it too.
        Consistency::Committed | Consistency::BoundedStale => Ok(()),
    }
}

/// Whether a dataset keeps a derived row.
///
/// `events` means every kind a producer sent, so it leaves out what TallyOwl
/// derived. `metric_points` keeps them, because a golden signal **is** a metric
/// point and an operator asking for metric points wants it.
fn keeps_derived(dataset: Dataset, row: &EventRow) -> bool {
    if dataset != Dataset::Events {
        return true;
    }
    !row.properties.contains_key(tallyowl_head_derived_key())
}

/// The property every derived row carries. It lives beside the rollup that
/// writes it; this names it once so the two cannot drift.
fn tallyowl_head_derived_key() -> &'static str {
    "derived"
}

/// The locator column and the bytes that a field and a literal name, when the
/// locator indexes that field.
///
/// The locator records a correlation column as its raw bytes and a property as
/// the display form of its value, because that is what `seal` writes. This has
/// to agree with `SegmentedStore::seal` exactly: a disagreement would prune to
/// the wrong segments and return **fewer** rows than the truth, which is the
/// worst failure class in FAILURE_MODES.md section 2.
fn locator_column(field: &expr::Field, value: &PropertyValue) -> Option<(String, Vec<u8>)> {
    use tallyowl_store::segment::schema;

    // A field reference that pins an origin or a type is asking a narrower
    // question than the locator can answer, so it reads the range instead.
    if field.origin.is_some() {
        return None;
    }
    let text = match value {
        PropertyValue::Null => return None,
        other => display_of(other),
    };

    match field.name.as_str() {
        // A trace or an event identifier travels as bytes in the row and as
        // hexadecimal in a query, so both forms are tried.
        "trace_id" | "event_id" => {
            let column = if field.name == "trace_id" {
                schema::TRACE_ID
            } else {
                schema::EVENT_ID
            };
            from_hex(&text).map(|bytes| (column.to_string(), bytes))
        }
        "session_id" => Some((schema::SESSION_ID.to_string(), text.into_bytes())),
        "request_id" => Some((schema::REQUEST_ID.to_string(), text.into_bytes())),
        // Every other name is a property, and every property is indexed. D20:
        // a dynamic scalar field defaults to exact `lookup` indexing, so an
        // unexpected correlation property stays instantly usable.
        //
        // The row's own columns are excluded: `kind`, `name`, `service_name`,
        // and `release` are low-cardinality, so a locator lookup would name
        // every segment and cost an index read for nothing.
        name if !ROW_COLUMNS.contains(&name) => Some((
            format!("{}{name}", schema::PROPERTY_PREFIX),
            text.into_bytes(),
        )),
        _ => None,
    }
}

/// Names that are the row's own columns rather than properties, and that hold
/// few enough distinct values that pruning on them buys nothing.
const ROW_COLUMNS: &[&str] = &[
    "kind",
    "name",
    "service_name",
    "release",
    "occurred_at",
    "received_at",
    "committed_at",
    "batch_id",
    "source_id",
    "project_id",
    "workspace_id",
];

fn display_of(value: &PropertyValue) -> String {
    match value {
        PropertyValue::Text(text) => text.clone(),
        PropertyValue::Decimal(text) => text.clone(),
        PropertyValue::Integer(v) => v.to_string(),
        PropertyValue::Unsigned(v) => v.to_string(),
        PropertyValue::Boolean(v) => v.to_string(),
        PropertyValue::Float(v) => crate::project::format_float(*v),
        PropertyValue::Bytes(v) => tallyowl_store::row::hex(v),
        PropertyValue::Null => String::new(),
    }
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || text.is_empty() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// One warning for each thing a coverage report says is worth saying.
///
/// A row with no identity cannot take part in an operator that correlates by
/// one, and dropping it in silence is how a number quietly becomes smaller than
/// the truth. This says how many, so a person can decide whether it matters.
/// Which identity a question means.
///
/// **Absent is latest-known**, and that is not an arbitrary default. It is the
/// answer somebody asking about an end user means, it is what every domain
/// operator used before the field existed, and it is the resolution a
/// conversion funnel needs: the steps before a sign-in were anonymous at the
/// time, and the person who bought is the person who clicked. L104 records that
/// a funnel built the other way counted a converting person twice.
///
/// A caller that asks for `event-time` gets who a row belonged to when it
/// happened, which is what a cohort question wants.
fn resolution_of(
    resolution: Option<&tallyowl_control_api::types::IdentityResolution>,
) -> crate::identity::Resolution {
    use tallyowl_control_api::types::IdentityResolution as Wire;
    match resolution {
        Some(Wire::EventTime) => crate::identity::Resolution::EventTime,
        Some(Wire::LatestKnown) | None => crate::identity::Resolution::LatestKnown,
    }
}

/// Which touch dimension an attribution breakdown asked for.
///
/// A breakdown names a field. The five this build groups by are the campaign
/// dimensions; anything else falls back to the campaign, because a credit list
/// grouped by an arbitrary property would be a different question with the same
/// name.
fn breakdown_dimension(
    breakdown: Option<&tallyowl_control_api::types::Dimension>,
) -> crate::campaign::Dimension {
    use crate::campaign::Dimension;
    match breakdown.map(|d| d.field.name.as_str()) {
        Some("campaign_channel") | Some("channel") => Dimension::Channel,
        Some("campaign_source") | Some("source") => Dimension::Source,
        Some("campaign_medium") | Some("medium") => Dimension::Medium,
        Some("campaign_content") | Some("content") => Dimension::Content,
        _ => Dimension::Campaign,
    }
}

/// What an attribution answer says beside its numbers.
///
/// Each of these is a fact a person needs to read the report correctly, and
/// each one was invisible without it: which model produced the numbers, how
/// much revenue nothing earned, how many repeat deliveries were folded, and
/// how many people the consent policy left out.
fn attribution_warnings(answer: &crate::attribution::Attribution) -> Vec<String> {
    let mut out = vec![format!(
        "This uses the `{}` model at version {}, under settings version {}.",
        answer.model.as_str(),
        answer.model_version,
        answer.settings_version
    )];
    if answer.unattributed > 0 {
        out.push(format!(
            "{} conversions worth {} had no touch inside the window. They are counted and credited to nothing, because crediting them would invent a touch nobody made.",
            answer.unattributed,
            answer.unattributed_value.to_text()
        ));
    }
    if answer.folded_orders > 0 {
        out.push(format!(
            "{} conversion rows carried an order another row already carried, and each order counts once.",
            answer.folded_orders
        ));
    }
    if answer.without_consent > 0 {
        out.push(format!(
            "{} conversions are not here because the person did not agree to marketing use. The collection policy of this project asks for that agreement.",
            answer.without_consent
        ));
    }
    if answer.coverage.rows_without_identity > 0 {
        out.push(format!(
            "{} rows carried no identity, so they could not be joined to a conversion. A campaign touch that belongs to nobody still measures the campaign.",
            answer.coverage.rows_without_identity
        ));
    }
    out
}

fn coverage_warnings(
    coverage: &crate::analysis::Coverage,
    basis: crate::identity::Basis,
) -> Vec<String> {
    let mut out = Vec::new();
    if coverage.rows_without_identity > 0 {
        out.push(format!(
            "{} items carried no {} and could not take part. A server-side item with no session and no end user is one of these.",
            coverage.rows_without_identity,
            match basis {
                crate::identity::Basis::EndUser => "end user",
                crate::identity::Basis::Session => "session",
                crate::identity::Basis::Group => "group",
            }
        ));
    }
    out
}

/// Walk a path tree into rows, parent first.
fn flatten_path(
    node: &crate::analysis::PathNode,
    parent: Option<&str>,
    into: &mut Vec<Vec<PropertyValue>>,
) {
    into.push(vec![
        PropertyValue::Unsigned(node.depth as u64),
        PropertyValue::Text(node.name.clone()),
        PropertyValue::Unsigned(node.count),
        parent
            .map(|name| PropertyValue::Text(name.to_string()))
            .unwrap_or(PropertyValue::Null),
    ]);
    for child in &node.children {
        flatten_path(child, Some(&node.name), into);
    }
}

/// A timeline cursor: an occurred time and an event ID.
///
/// `docs/QUERY.md` section 13: "A sort without a total order appends the event
/// ID as a final key. The order is therefore deterministic." The cursor carries
/// both halves of that key, so resuming cannot repeat a row or skip one.
fn write_cursor(at: i64, event_id: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&at.to_be_bytes());
    out.extend_from_slice(event_id);
    out
}

fn read_cursor(bytes: &[u8]) -> Option<(i64, [u8; 16])> {
    if bytes.len() != 24 {
        return None;
    }
    let at = i64::from_be_bytes(bytes[..8].try_into().ok()?);
    let event_id: [u8; 16] = bytes[8..].try_into().ok()?;
    Some((at, event_id))
}

/// The stored name of one telemetry kind, from the control package's copy of
/// the enumeration.
///
/// Every entry specification includes `types/common.csil`, so each generated
/// package holds its own copy of the same shape. The names are the contract's;
/// only the Rust type differs. See [`crate::wire`], which does the same for the
/// error taxonomy.
fn control_kind_name(kind: &tallyowl_control_api::types::TelemetryKind) -> &'static str {
    use tallyowl_control_api::types::TelemetryKind as K;
    match kind {
        K::Event => "event",
        K::PageView => "page-view",
        K::SessionStart => "session-start",
        K::SessionEnd => "session-end",
        K::SessionHeartbeat => "session-heartbeat",
        K::Interaction => "interaction",
        K::FeatureExposure => "feature-exposure",
        K::Identify => "identify",
        K::Alias => "alias",
        K::Group => "group",
        K::Conversion => "conversion",
        K::Error => "error",
        K::Span => "span",
        K::MetricPoint => "metric-point",
        K::CampaignTouch => "campaign-touch",
        K::CampaignCost => "campaign-cost",
    }
}

impl QueryService {
    /// The identity graph for one project, over everything it holds.
    ///
    /// An erasure needs it: removing a person means removing every timeline
    /// that resolved to them, and the links that say which those are can be at
    /// any time. So this reads the whole range rather than a window, which is
    /// the honest cost of getting a removal right.
    pub fn identity_of(
        &self,
        project_id: [u8; 16],
    ) -> Result<crate::identity::Identity, TallyOwlError> {
        let scanned = self
            .store
            .scan(
                project_id,
                i64::MIN / 2,
                i64::MAX / 2,
                TimeBasis::OccurredAt,
            )
            .map_err(crate::ingest::to_service_error)?;
        if scanned.incomplete {
            // A graph built from part of the data would name part of a person,
            // and an erasure from it would leave the rest behind. That is worse
            // than refusing.
            return Err(TallyOwlError::new(
                tallyowl_obs::ErrorCode::IncompleteResult,
                format!(
                    "Part of this project's stored data could not be read, so we cannot say which identities belong to one person. An erasure from an incomplete picture would remove part of somebody.{}",
                    self.damage()
                ),
            ));
        }
        Ok(crate::identity::Identity::build(project_id, &scanned.rows))
    }
}

// ---------------------------------------------------------------------------
// Partial states, so an aggregate is computed where the rows are
// ---------------------------------------------------------------------------
//
// L101 built the fan-out and said plainly what it did not build: the general
// aggregate is computed in the coordinator, so a scan that feeds one **moves
// rows**. `docs/QUERY.md` section 5 says the coordinator merges partial states
// and never pulls raw rows to aggregate, and this is the part that was missing.
//
// **There is one aggregation and this is it.** A tablet answers a partial
// aggregate by running the same `Accumulator` over its own rows, because a
// second implementation of a sum is a second answer waiting to happen. L102 is
// what that costs.
//
// **Not every measure has a partial state**, which is what L101 said would
// decide the shape. A sum, a count, a minimum, a maximum, an average, and an
// exact distinct count all merge. A quantile over a histogram merges only when
// the layouts agree, and a counter rate needs its samples. Those two carry
// their state as well, and a measure that cannot say what it holds refuses the
// push-down rather than answering from one tablet's rows.

impl Accumulator {
    /// What this accumulator holds, as a value another one can take back.
    ///
    /// `None` means this measure has no mergeable partial state, and the
    /// coordinator then asks for rows instead. Saying so is the whole contract:
    /// a partial state that merged wrongly would be a wrong number nobody could
    /// reconcile.
    fn partial_state(&self) -> Option<CborValue> {
        let mut map = MapBuilder::new();
        Some(match self {
            Accumulator::Count(n) => map
                .put("t", CborValue::text("count"))
                .put("n", CborValue::Unsigned(*n))
                .build(),
            Accumulator::Sum {
                exact,
                inexact,
                saw_float,
                saw_any,
            } => {
                map = map
                    .put("t", CborValue::text("sum"))
                    .put("f", CborValue::Float(*inexact))
                    .put("sf", CborValue::Bool(*saw_float))
                    .put("sa", CborValue::Bool(*saw_any));
                if let Some((units, scale)) = exact {
                    map = map
                        .put("u", CborValue::text(units.to_string()))
                        .put("s", CborValue::Unsigned(*scale as u64));
                }
                map.build()
            }
            Accumulator::Extreme { largest, held } => {
                map = map
                    .put("t", CborValue::text("extreme"))
                    .put("l", CborValue::Bool(*largest));
                if let Some(held) = held {
                    map = map.put("v", property_to_cbor(held));
                }
                map.build()
            }
            Accumulator::Average { total, count } => map
                .put("t", CborValue::text("average"))
                .put("s", CborValue::Float(*total))
                .put("n", CborValue::Unsigned(*count))
                .build(),
            Accumulator::Distinct(values) => map
                .put("t", CborValue::text("distinct"))
                .put(
                    "v",
                    CborValue::Array(
                        values
                            .iter()
                            .map(|key| property_to_cbor(&key.value()))
                            .collect(),
                    ),
                )
                .build(),
            // A counter rate and a histogram both have a state and neither is a
            // number. They are not pushed down in this release, and the
            // coordinator asks for rows, which is what it did for every measure
            // before this existed.
            Accumulator::Counter { .. } | Accumulator::Histogram { .. } => return None,
            Accumulator::Unsupported(_) => return None,
        })
    }

    /// Take another tablet's partial state into this one.
    fn merge_partial(&mut self, state: &CborValue) -> Result<(), TallyOwlError> {
        let kind = state.field("t").and_then(CborValue::as_text).unwrap_or("");
        match (self, kind) {
            (Accumulator::Count(held), "count") => {
                *held += state.field("n").and_then(CborValue::as_unsigned).unwrap_or(0);
            }
            (
                Accumulator::Sum {
                    exact,
                    inexact,
                    saw_float,
                    saw_any,
                },
                "sum",
            ) => {
                *inexact += state.field("f").and_then(CborValue::as_float).unwrap_or(0.0);
                *saw_float |= state.field("sf").and_then(CborValue::as_bool).unwrap_or(false);
                *saw_any |= state.field("sa").and_then(CborValue::as_bool).unwrap_or(false);
                let arrived = state
                    .field("u")
                    .and_then(CborValue::as_text)
                    .and_then(|text| text.parse::<i128>().ok())
                    .zip(state.field("s").and_then(CborValue::as_unsigned));
                match (exact.as_mut(), arrived) {
                    (Some(held), Some((units, scale))) => {
                        // The two exact sums are aligned to the wider scale, so
                        // 1.5 and 1.50 add to 3.00 and not to 16.5.
                        let scale = scale as u32;
                        let widest = held.1.max(scale);
                        let lift = |units: i128, from: u32| {
                            units * 10i128.pow(widest - from)
                        };
                        *held = (lift(held.0, held.1) + lift(units, scale), widest);
                    }
                    // One end saw a float, so the exact sum is gone for both.
                    _ => *exact = None,
                }
            }
            (Accumulator::Extreme { largest, held }, "extreme") => {
                if let Some(arrived) = state.field("v").and_then(cbor_to_property) {
                    let take = match held.as_ref() {
                        None => true,
                        Some(current) => match expr::compare(&arrived, current) {
                            Some(std::cmp::Ordering::Greater) => *largest,
                            Some(std::cmp::Ordering::Less) => !*largest,
                            _ => false,
                        },
                    };
                    if take {
                        *held = Some(arrived);
                    }
                }
            }
            (Accumulator::Average { total, count }, "average") => {
                *total += state.field("s").and_then(CborValue::as_float).unwrap_or(0.0);
                *count += state.field("n").and_then(CborValue::as_unsigned).unwrap_or(0);
            }
            (Accumulator::Distinct(values), "distinct") => {
                for value in state.field("v").and_then(CborValue::as_array).unwrap_or(&[]) {
                    if let Some(property) = cbor_to_property(value) {
                        values.insert(GroupKey::of(Some(&property)));
                    }
                }
            }
            (_, other) => {
                return Err(TallyOwlError::internal(format!(
                    "A tablet sent a partial state of kind `{other}` that does not belong to the measure it answered. This is a defect in TallyOwl rather than in the query."
                )))
            }
        }
        Ok(())
    }
}

/// A property value as CBOR, and back. It carries the type as well as the
/// bytes, because a minimum over text and a minimum over a number are different
/// answers and a partial state that lost the difference would merge them.
fn property_to_cbor(value: &PropertyValue) -> CborValue {
    let mut map = MapBuilder::new().put("k", CborValue::text(value.type_name()));
    map = match value {
        PropertyValue::Null => map,
        PropertyValue::Boolean(v) => map.put("v", CborValue::Bool(*v)),
        PropertyValue::Integer(v) => map.put("v", CborValue::integer(*v)),
        PropertyValue::Unsigned(v) => map.put("v", CborValue::Unsigned(*v)),
        PropertyValue::Float(v) => map.put("v", CborValue::Float(*v)),
        PropertyValue::Decimal(v) | PropertyValue::Text(v) => map.put("v", CborValue::text(v)),
        PropertyValue::Bytes(v) => map.put("v", CborValue::Bytes(v.clone())),
    };
    map.build()
}

fn cbor_to_property(value: &CborValue) -> Option<PropertyValue> {
    let kind = value.field("k").and_then(CborValue::as_text)?;
    let held = value.field("v");
    Some(match kind {
        "null" => PropertyValue::Null,
        "boolean" => PropertyValue::Boolean(held.and_then(CborValue::as_bool)?),
        "integer" => PropertyValue::Integer(held.and_then(CborValue::as_integer)?),
        "unsigned" => PropertyValue::Unsigned(held.and_then(CborValue::as_unsigned)?),
        "float" => PropertyValue::Float(held.and_then(CborValue::as_float)?),
        "decimal" => PropertyValue::Decimal(held.and_then(CborValue::as_text)?.to_string()),
        "text" => PropertyValue::Text(held.and_then(CborValue::as_text)?.to_string()),
        "bytes" => PropertyValue::Bytes(held.and_then(CborValue::as_bytes)?.to_vec()),
        _ => return None,
    })
}

/// Every group's partial state, as bytes a coordinator can merge.
///
/// `None` when a measure in this aggregate has no partial state. The caller
/// then asks for rows, which is what it did for every measure before the
/// push-down existed.
fn encode_partial_groups(groups: &BTreeMap<Vec<GroupKey>, Vec<Accumulator>>) -> Option<Vec<u8>> {
    let mut out: Vec<CborValue> = Vec::with_capacity(groups.len());
    for (key, accumulators) in groups {
        let mut states: Vec<CborValue> = Vec::with_capacity(accumulators.len());
        for accumulator in accumulators {
            states.push(accumulator.partial_state()?);
        }
        out.push(
            MapBuilder::new()
                .put(
                    "k",
                    CborValue::Array(
                        key.iter()
                            .map(|part| property_to_cbor(&part.value()))
                            .collect(),
                    ),
                )
                .put("s", CborValue::Array(states))
                .build(),
        );
    }
    Some(cbor_encode(&CborValue::Array(out)))
}

/// Take one tablet's partial states into the groups being built.
fn merge_partial_groups(
    into: &mut BTreeMap<Vec<GroupKey>, Vec<Accumulator>>,
    bytes: &[u8],
    measures: &[Measure],
) -> Result<(), TallyOwlError> {
    let decoded = cbor_decode(bytes).map_err(|e| {
        TallyOwlError::internal(format!("A partial aggregate could not be read: {e}"))
    })?;
    for group in decoded.as_array().unwrap_or(&[]) {
        let key: Vec<GroupKey> = group
            .field("k")
            .and_then(CborValue::as_array)
            .unwrap_or(&[])
            .iter()
            .map(|part| GroupKey::of(cbor_to_property(part).as_ref()))
            .collect();
        let states = group
            .field("s")
            .and_then(CborValue::as_array)
            .unwrap_or(&[]);
        if states.len() != measures.len() {
            return Err(TallyOwlError::internal(format!(
                "A tablet answered with {} partial states for {} measures. This is a defect in TallyOwl rather than in the query.",
                states.len(),
                measures.len()
            )));
        }
        let held = into
            .entry(key)
            .or_insert_with(|| measures.iter().map(Accumulator::new).collect());
        for (index, state) in states.iter().enumerate() {
            held[index].merge_partial(state)?;
        }
    }
    Ok(())
}

/// Fold rows into groups. The one place a measure meets a row.
fn fold_rows(
    groups: &mut BTreeMap<Vec<GroupKey>, Vec<Accumulator>>,
    rows: &[EventRow],
    basis: TimeBasis,
    bucketing: &Option<Bucketing>,
    dimensions: &[(String, expr::Field)],
    measures: &[Measure],
) -> Result<(), TallyOwlError> {
    for row in rows {
        let mut key: Vec<GroupKey> = Vec::new();
        if let Some(bucket) = bucketing {
            key.push(GroupKey::Integer(bucket.start_of(time_of(row, basis))));
        }
        for (_, field) in dimensions {
            key.push(GroupKey::of(field.read(row).value()));
        }
        let entry = groups
            .entry(key)
            .or_insert_with(|| measures.iter().map(Accumulator::new).collect());
        for (index, measure) in measures.iter().enumerate() {
            entry[index].add(measure, row)?;
        }
    }
    Ok(())
}
