//! Attribution: which touch earned a conversion, and how much of its value.
//!
//! `docs/DATA_MODEL.md` section 3.6 names the six models and `docs/QUERY.md`
//! section 12.6 gives the operator its shape. D40 states the property that
//! decides how this module is built:
//!
//! > Attribution is a pure function over immutable touches. A model
//! > parameter is configuration, not code, and not a schema decision.
//!
//! Nothing here writes. A model change, a weight change, and a window change
//! each produce a different answer from the same stored rows, and the stored
//! rows never move. That is the Phase 9 exit criterion "model changes recompute
//! from immutable facts", and it is a property of this module having no output
//! other than its return value.
//!
//! # The five decisions this module makes, and why
//!
//! **A person is correlated by latest-known identity.** Somebody clicks a
//! campaign anonymously, comes back a week later, signs in, and buys. Event-time
//! identity would make the touch and the conversion two different people and
//! every campaign would show nothing converting. This is the same defect L104
//! found in the funnel, and it is the same fix.
//!
//! **A touch is a `campaign-touch` row, or a page view that carried
//! campaign parameters.** A marketing site sends a page view when somebody
//! lands on it, and that page view carries the campaign parameters out of the
//! address. A referrer on its own is not enough: a browser sends one for a link
//! inside the application too, and taking those would give every internal
//! navigation a share of the revenue. Requiring a second explicit touch beside it would mean every
//! landing page had to send two items to be measured. Two items that describe
//! one landing are folded into one touch; see [`touches`].
//!
//! **A conversion with an order identifier is idempotent.** A checkout that
//! retried, a webhook that arrived twice, and a refreshed receipt page produce
//! one conversion. The earliest one wins, so a repeat cannot move a conversion
//! into a later attribution window. A conversion with no order identifier is its
//! own event, because nothing says otherwise.
//!
//! **Consent is checked where campaign data joins a person.** D30: "Consent
//! applies at the point where campaign data joins an identified end user." A
//! person who denied marketing consent is excluded from attribution when the
//! project's policy asks for consent, and their campaign facts are still
//! counted, because a campaign fact tied to nobody is not personal data.
//!
//! **The credited value adds up.** [`crate::money`] divides an exact decimal by
//! weights and loses nothing. A campaign column that did not total the revenue
//! it came from is a report somebody has to explain.

use std::collections::{BTreeMap, BTreeSet};

use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_store::row::{EventRow, PropertyValue};

use crate::analysis::Coverage;
use crate::campaign::{self, Channel, Dimension, Touch};
use crate::expr::Prepared;
use crate::identity::{Basis, Identity, Resolution};
use crate::money::Amount;

/// Which model version produced a result.
///
/// It rises when a rule inside a model changes, and never when a weight
/// changes: a weight is configuration and its own version travels beside this
/// one. A result names both, so nothing that was computed under different rules
/// can be mistaken for one number.
pub const MODEL_VERSION: u64 = 1;

/// The credit rule a question asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Model {
    FirstTouch,
    LastTouch,
    LastNonDirect,
    Linear,
    Position,
    Decay,
}

impl Model {
    pub fn as_str(&self) -> &'static str {
        match self {
            Model::FirstTouch => "first-touch",
            Model::LastTouch => "last-touch",
            Model::LastNonDirect => "last-non-direct",
            Model::Linear => "linear",
            Model::Position => "position",
            Model::Decay => "decay",
        }
    }

    pub fn parse(name: &str) -> Option<Model> {
        Some(match name {
            "first-touch" => Model::FirstTouch,
            "last-touch" => Model::LastTouch,
            "last-non-direct" => Model::LastNonDirect,
            "linear" => Model::Linear,
            "position" => Model::Position,
            "decay" => Model::Decay,
            _ => return None,
        })
    }

    /// Every model this build knows.
    pub fn all() -> [Model; 6] {
        [
            Model::FirstTouch,
            Model::LastTouch,
            Model::LastNonDirect,
            Model::Linear,
            Model::Position,
            Model::Decay,
        ]
    }
}

/// What an operator configures for one project. D40.
///
/// Every field is a number an operator can change, and a change recomputes
/// rather than migrating anything.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub position_first_weight: f64,
    pub position_last_weight: f64,
    pub decay_half_life_ms: i64,
    pub lookback_ms: i64,
    pub enabled_models: BTreeSet<Model>,
    pub touch_retention_ms: i64,
    /// Rises on every change, so two results under different weights are two
    /// different answers rather than one number that moved.
    pub version: u64,
}

/// The shipped defaults.
///
/// **D40 left these to Phase 9 and this is Phase 9 selecting them.** Each one
/// is written with the reason it holds, because a default nobody can argue with
/// is a default nobody can change with confidence:
///
/// - **40 percent to the first touch and 40 to the last.** The two ends are the
///   discovery and the decision, and the 20 percent in between is what the
///   middle did. Splitting the ends evenly with the middle would make a
///   five-touch journey credit the discovery the same as a reminder;
/// - **a seven-day half-life.** A touch a week before a purchase carries half
///   the credit of one on the day. Shorter makes anything but the last week
///   invisible; longer makes a decay model behave like a linear one;
/// - **a thirty-day lookback.** It is the window most purchase decisions fit
///   inside, and it is short enough that a person can hold the journey in their
///   head when they read the report;
/// - **ninety days of touch retention.** Three times the lookback, so an
///   operator can widen the window twice before the coupling in POLICY.md
///   section 5 refuses the query;
/// - **every model enabled.** A model nobody enabled cannot be compared against
///   the one they did, and comparing them is the whole reason there are six.
impl Default for Settings {
    fn default() -> Settings {
        Settings {
            position_first_weight: 0.4,
            position_last_weight: 0.4,
            decay_half_life_ms: 7 * 86_400_000,
            lookback_ms: 30 * 86_400_000,
            enabled_models: Model::all().into_iter().collect(),
            touch_retention_ms: 90 * 86_400_000,
            version: 0,
        }
    }
}

impl Settings {
    /// Whether these settings could be applied.
    ///
    /// Checked before they are stored, for the same reason a policy document is:
    /// settings that would refuse every question are settings an operator
    /// believes are set.
    pub fn check(&self) -> Result<(), TallyOwlError> {
        for (name, weight) in [
            ("first", self.position_first_weight),
            ("last", self.position_last_weight),
        ] {
            if !weight.is_finite() || !(0.0..=1.0).contains(&weight) {
                return Err(TallyOwlError::invalid_argument(format!(
                    "The position weight for the {name} touch is between 0 and 1, and this one is {weight}."
                )));
            }
        }
        let ends = self.position_first_weight + self.position_last_weight;
        if ends > 1.0 {
            return Err(TallyOwlError::invalid_argument(format!(
                "The first and last position weights add up to {ends}, and there would be nothing left for the touches in between. Use weights that add up to 1 or less."
            )));
        }
        if self.decay_half_life_ms <= 0 {
            return Err(TallyOwlError::invalid_argument(
                "A decay half-life has to be more than nothing. With no half-life every touch would carry the same credit, which is the linear model.",
            ));
        }
        if self.lookback_ms <= 0 {
            return Err(TallyOwlError::invalid_argument(
                "An attribution lookback has to be more than nothing. Without one no touch could ever be inside the window.",
            ));
        }
        if self.touch_retention_ms <= 0 {
            return Err(TallyOwlError::invalid_argument(
                "A touch retention has to be more than nothing.",
            ));
        }
        if self.enabled_models.is_empty() {
            return Err(TallyOwlError::invalid_argument(
                "These settings enable no attribution model, so every attribution question would be refused. Enable at least one.",
            ));
        }
        // The coupling POLICY.md section 5 calls a correctness rule. A lookback
        // longer than the retained range moves credit to later touches, because
        // the early ones aged out, and the result looks correct and is wrong.
        if self.lookback_ms > self.touch_retention_ms {
            return Err(TallyOwlError::invalid_argument(format!(
                "The default lookback is {} ms and touches are kept for {} ms. A lookback longer than the retained range moves credit to later touches, because the early ones are gone. Shorten the lookback, or keep touches for longer.",
                self.lookback_ms, self.touch_retention_ms
            )));
        }
        Ok(())
    }

    /// Refuse a window this project cannot answer honestly. D40.
    pub fn check_window(&self, lookback_ms: i64) -> Result<(), TallyOwlError> {
        if lookback_ms <= 0 {
            return Err(TallyOwlError::invalid_argument(
                "An attribution question needs a lookback window. Without one no touch is inside it.",
            ));
        }
        if lookback_ms > self.touch_retention_ms {
            return Err(TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!(
                    "This question looks back {lookback_ms} ms and this project keeps touches for {} ms. The touches at the start of that window are gone, so the credit they earned would move to later touches and the answer would look correct and be wrong. Shorten the window to {} ms or less.",
                    self.touch_retention_ms, self.touch_retention_ms
                ),
            )
            .retryable(false));
        }
        Ok(())
    }

    pub fn check_model(&self, model: Model) -> Result<(), TallyOwlError> {
        if !self.enabled_models.contains(&model) {
            return Err(TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!(
                    "This project does not answer the `{}` model. It answers {}.",
                    model.as_str(),
                    self.enabled_names().join(", ")
                ),
            )
            .retryable(false));
        }
        Ok(())
    }

    pub fn enabled_names(&self) -> Vec<String> {
        self.enabled_models
            .iter()
            .map(|m| m.as_str().to_string())
            .collect()
    }
}

/// What one attribution question asks.
pub struct Question {
    /// The conversion goal, which is the stored row's name.
    pub goal: String,
    pub model: Model,
    pub lookback_ms: i64,
    /// Which touch dimension the answer is grouped by.
    pub dimension: Dimension,
    /// Keep only touches this expression accepts.
    pub touch_filter: Option<Prepared>,
    /// Whether a person who denied marketing consent takes part. D30.
    pub needs_consent: bool,
    /// Which identity correlates a touch with a conversion.
    ///
    /// Latest-known unless a caller asks otherwise: somebody clicks a campaign
    /// anonymously, comes back, signs in, and buys, and event-time identity
    /// would make that two people. See the module note and L104.
    pub resolution: Resolution,
}

/// One touch, as attribution sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Touchpoint {
    pub event_id: [u8; 16],
    pub occurred_at: i64,
    pub touch: Touch,
    pub channel: Channel,
    /// The identity this touch belonged to, under the question's resolution.
    pub key: String,
}

/// One conversion, after the idempotent fold.
#[derive(Debug, Clone, PartialEq)]
pub struct Conversion {
    pub event_id: [u8; 16],
    pub occurred_at: i64,
    pub key: String,
    pub value: Amount,
    pub currency: Option<String>,
    pub order_id: Option<String>,
    /// How many rows carried this one order. More than one is an idempotent
    /// fold rather than a duplicate delivery, and the number is reported.
    pub rows: u64,
}

/// One row of an attribution answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Credit {
    /// The dimension value: the campaign, the channel, the source, and so on.
    pub dimension: String,
    /// The exact credited value.
    pub value: Amount,
    /// The share of conversions credited to this row, as a count of whole
    /// conversions where the model gave one touch everything, and as a fraction
    /// otherwise.
    pub conversions: f64,
    /// How many touches of this dimension took part.
    pub touches: u64,
    /// How many of those touches took **no** credit under this model.
    ///
    /// `docs/DATA_MODEL.md` section 6 calls these assisted conversions. Under
    /// `first-touch`, every touch but the first is an assist; under `linear`,
    /// nothing is, because every touch takes a share. An assist is what a
    /// single-touch model hides: the campaigns that were part of the journey
    /// and earned nothing for it.
    pub assists: u64,
}

/// The whole answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribution {
    pub model: Model,
    pub model_version: u64,
    pub settings_version: u64,
    pub rows: Vec<Credit>,
    /// The exact total, which is what the credited rows add up to.
    pub total_value: Amount,
    pub conversions: u64,
    /// Conversions with no touch inside the window. They are counted and never
    /// credited, because crediting them to nothing would make the report add up
    /// to less than the revenue and crediting them to "direct" would invent a
    /// touch nobody made.
    pub unattributed: u64,
    /// The exact value of those conversions.
    pub unattributed_value: Amount,
    /// Conversion rows that another row of the same order already carried.
    pub folded_orders: u64,
    /// Conversions left out because the person denied marketing consent.
    pub without_consent: u64,
    pub currency: Option<String>,
    pub coverage: Coverage,
}

impl Attribution {
    /// Every credited row plus the uncredited value. It equals the revenue the
    /// question covered, and a test asserts it.
    pub fn accounted(&self) -> Amount {
        self.rows.iter().fold(self.unattributed_value, |sum, row| {
            sum.add(&row.value).unwrap_or(sum)
        })
    }
}

/// Which consent state a row carried. D30 keeps it with the event.
pub const CONSENT_MARKETING: &str = "consent_marketing";
pub const CONSENT_ANALYTICS: &str = "consent_analytics";
pub const CONSENT_POLICY_VERSION: &str = "consent_policy_version";

/// Whether this row's person agreed to marketing use.
///
/// An absent consent state is **not** a denial. D30: "TallyOwl does not guess a
/// jurisdiction and does not change behavior by geography," and an application
/// that never sent a consent state has not said no on its person's behalf. An
/// installation that wants the stricter reading turns on
/// `attribution_needs_consent` and gets it, which is the operator making the
/// choice with knowledge TallyOwl does not have.
pub fn marketing_consent(row: &EventRow) -> Option<bool> {
    match row.properties.get(CONSENT_MARKETING) {
        Some((PropertyValue::Text(state), _)) => match state.as_str() {
            "granted" => Some(true),
            "denied" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// The touches in a set of rows, in time order.
///
/// A `campaign-touch` row is always a touch. A `page-view` row is one when it
/// carried a campaign or a referrer, because a marketing landing page sends a
/// page view and the campaign parameters come out of the address it was opened
/// with.
///
/// **Two rows that describe one landing become one touch.** A client that sends
/// both an explicit touch and the page view beside it would otherwise be
/// counted twice, and every linear result from that client would be wrong by a
/// factor a person could not see. Two rows fold when they belong to one person,
/// carry the same campaign parameters, and are within [`SAME_LANDING_MS`] of
/// each other.
pub const SAME_LANDING_MS: i64 = 2_000;

pub fn touches(
    rows: &[EventRow],
    identity: &Identity,
    question: &Question,
    coverage: &mut Coverage,
) -> Vec<Touchpoint> {
    let mut out: Vec<Touchpoint> = Vec::new();
    for row in rows {
        if !is_touch(row) {
            continue;
        }
        if let Some(filter) = &question.touch_filter {
            if !filter.keeps(row) {
                continue;
            }
        }
        let touch = Touch::of(row);
        // A page view with no campaign tag is not a touch. It is somebody
        // moving around inside the application, and a browser sends a referrer
        // for that as readily as for a link from outside. See
        // [`Touch::has_campaign`].
        if row.kind == "page-view" && !touch.has_campaign() {
            continue;
        }
        let Some(key) = identity.key_for(row, Basis::EndUser, question.resolution) else {
            // A touch that belongs to nobody still measures the campaign, and
            // it can never be joined to a conversion. It is counted here and
            // the count reaches the answer.
            coverage.rows_without_identity += 1;
            continue;
        };
        out.push(Touchpoint {
            event_id: row.event_id,
            occurred_at: row.occurred_at,
            channel: campaign::classify(&touch),
            touch,
            key,
        });
    }

    // Time order, then event ID, so one set of rows always gives one order.
    out.sort_by(|a, b| {
        a.key
            .cmp(&b.key)
            .then(a.occurred_at.cmp(&b.occurred_at))
            .then(a.event_id.cmp(&b.event_id))
    });
    fold_same_landing(out)
}

fn is_touch(row: &EventRow) -> bool {
    matches!(row.kind.as_str(), "campaign-touch" | "page-view")
}

/// Fold two rows that describe one landing into one touch.
fn fold_same_landing(sorted: Vec<Touchpoint>) -> Vec<Touchpoint> {
    let mut out: Vec<Touchpoint> = Vec::with_capacity(sorted.len());
    for touch in sorted {
        let folds = out.last().is_some_and(|held: &Touchpoint| {
            held.key == touch.key
                && touch.occurred_at - held.occurred_at <= SAME_LANDING_MS
                && held.touch.source == touch.touch.source
                && held.touch.medium == touch.touch.medium
                && held.touch.campaign == touch.touch.campaign
                && held.touch.click_id == touch.touch.click_id
        });
        if folds {
            continue;
        }
        out.push(touch);
    }
    out
}

/// The conversions in a set of rows, folded by order.
///
/// The earliest row of one order wins. A later repeat cannot move a conversion
/// forward in time, which matters because the conversion time is what the
/// lookback window is measured back from: a duplicate that arrived a day late
/// would otherwise widen the window and change which touches earned it.
pub fn conversions(
    rows: &[EventRow],
    identity: &Identity,
    question: &Question,
    coverage: &mut Coverage,
) -> Result<(Vec<Conversion>, u64, u64), TallyOwlError> {
    let mut held: BTreeMap<String, Conversion> = BTreeMap::new();
    let mut standalone: Vec<Conversion> = Vec::new();
    let mut folded = 0u64;
    let mut without_consent = 0u64;

    for row in rows {
        if row.kind != "conversion" || row.name != question.goal {
            continue;
        }
        if question.needs_consent && marketing_consent(row) == Some(false) {
            without_consent += 1;
            continue;
        }
        let Some(key) = identity.key_for(row, Basis::EndUser, question.resolution) else {
            coverage.rows_without_identity += 1;
            continue;
        };
        let value = value_of(row)?;
        let conversion = Conversion {
            event_id: row.event_id,
            occurred_at: row.occurred_at,
            key,
            value,
            currency: text_of(row, "currency"),
            order_id: text_of(row, "order_id"),
            rows: 1,
        };
        match &conversion.order_id {
            None => standalone.push(conversion),
            Some(order_id) => {
                let at = format!("{}\u{0}{order_id}", question.goal);
                match held.get_mut(&at) {
                    None => {
                        held.insert(at, conversion);
                    }
                    Some(first) => {
                        folded += 1;
                        first.rows += 1;
                        // The earliest wins, and a tie goes to the lower event
                        // identifier so two runs of one query agree.
                        let earlier = (conversion.occurred_at, conversion.event_id)
                            < (first.occurred_at, first.event_id);
                        if earlier {
                            let rows = first.rows;
                            *first = conversion;
                            first.rows = rows;
                        }
                    }
                }
            }
        }
    }

    let mut out: Vec<Conversion> = held.into_values().chain(standalone).collect();
    out.sort_by(|a, b| {
        a.occurred_at
            .cmp(&b.occurred_at)
            .then(a.event_id.cmp(&b.event_id))
    });
    Ok((out, folded, without_consent))
}

fn value_of(row: &EventRow) -> Result<Amount, TallyOwlError> {
    match row.properties.get("value") {
        None => Ok(Amount::ZERO),
        Some((PropertyValue::Decimal(text), _)) => Amount::parse(text).ok_or_else(|| {
            TallyOwlError::invalid_argument(format!(
                "The conversion `{}` carries the value `{text}`, which is not an exact amount this build can divide. A credited value has to add up to the value it came from.",
                row.event_id_text()
            ))
        }),
        Some((PropertyValue::Integer(value), _)) => Ok(Amount::new(*value as i128, 0)),
        Some((PropertyValue::Unsigned(value), _)) => Ok(Amount::new(*value as i128, 0)),
        Some((held, _)) => Err(TallyOwlError::invalid_argument(format!(
            "The conversion `{}` carries a {} value. Money is an exact decimal, never a float, so this one cannot be credited.",
            row.event_id_text(),
            held.type_name()
        ))),
    }
}

fn text_of(row: &EventRow, key: &str) -> Option<String> {
    match row.properties.get(key) {
        Some((PropertyValue::Text(value), _)) if !value.trim().is_empty() => {
            Some(value.trim().to_string())
        }
        _ => None,
    }
}

/// The weights one model gives a journey, in the journey's own order.
///
/// The whole of each model is here, in one function, so the six can be read
/// against one another. Each returns a weight for each touch, and the caller
/// normalizes: [`crate::money::Amount::split`] treats weights as shares, so a
/// model never has to make them add up to one.
pub fn weights(model: Model, journey: &[&Touchpoint], at: i64, settings: &Settings) -> Vec<f64> {
    let count = journey.len();
    if count == 0 {
        return Vec::new();
    }
    match model {
        Model::FirstTouch => one_of(count, 0),
        Model::LastTouch => one_of(count, count - 1),
        Model::LastNonDirect => {
            // The last touch that was not direct. A journey that is direct all
            // the way through falls back to the last touch, because the
            // alternative is to credit nothing and lose the revenue out of the
            // report. The fallback is named in the result's warnings.
            let at = journey
                .iter()
                .rposition(|touch| !touch.channel.is_direct())
                .unwrap_or(count - 1);
            one_of(count, at)
        }
        Model::Linear => vec![1.0; count],
        Model::Position => position_weights(count, settings),
        Model::Decay => journey
            .iter()
            .map(|touch| {
                let age = (at - touch.occurred_at).max(0) as f64;
                let half_life = settings.decay_half_life_ms.max(1) as f64;
                0.5f64.powf(age / half_life)
            })
            .collect(),
    }
}

fn one_of(count: usize, at: usize) -> Vec<f64> {
    let mut out = vec![0.0; count];
    out[at] = 1.0;
    out
}

/// The position model's weights.
///
/// One touch takes everything. Two touches share what the two ends were given,
/// in proportion, because there is no middle to hold the rest and rescaling to
/// the ends is what an operator who set 40 and 40 meant. Three or more give the
/// ends their weights and divide the remainder evenly between the touches in
/// between.
fn position_weights(count: usize, settings: &Settings) -> Vec<f64> {
    let first = settings.position_first_weight;
    let last = settings.position_last_weight;
    match count {
        1 => vec![1.0],
        2 => {
            let ends = first + last;
            if ends <= 0.0 {
                vec![1.0, 1.0]
            } else {
                vec![first / ends, last / ends]
            }
        }
        _ => {
            let middle_total = (1.0 - first - last).max(0.0);
            let each = middle_total / (count - 2) as f64;
            let mut out = vec![each; count];
            out[0] = first;
            out[count - 1] = last;
            out
        }
    }
}

/// Run one attribution question.
///
/// `rows` is one project's rows over the range, already deduplicated and
/// authorized. `identity` is built from the same rows plus the lookback the
/// query service applies.
pub fn run(
    rows: &[EventRow],
    incomplete: bool,
    identity: &Identity,
    question: &Question,
    settings: &Settings,
) -> Result<Attribution, TallyOwlError> {
    settings.check_model(question.model)?;
    settings.check_window(question.lookback_ms)?;

    let mut coverage = Coverage {
        incomplete,
        ..Coverage::default()
    };

    let touches = touches(rows, identity, question, &mut coverage);
    let (conversions, folded_orders, without_consent) =
        conversions(rows, identity, question, &mut coverage)?;

    // The touches of one person, in time order. A journey is a slice of this.
    let mut by_key: BTreeMap<&str, Vec<&Touchpoint>> = BTreeMap::new();
    for touch in &touches {
        by_key.entry(touch.key.as_str()).or_default().push(touch);
    }
    coverage.keys = by_key.len();

    // Dimension value to credited amount, credited conversions, touches, and
    // assists.
    let mut credited: BTreeMap<String, (Amount, f64, u64, u64)> = BTreeMap::new();
    let mut total = Amount::ZERO;
    let mut unattributed = 0u64;
    let mut unattributed_value = Amount::ZERO;
    let mut currency: Option<String> = None;
    let mut mixed_currency = false;

    for conversion in &conversions {
        total = total.add(&conversion.value).ok_or_else(too_much)?;
        match (&currency, &conversion.currency) {
            (None, Some(held)) => currency = Some(held.clone()),
            (Some(held), Some(seen)) if held != seen => mixed_currency = true,
            _ => {}
        }

        let journey: Vec<&Touchpoint> = by_key
            .get(conversion.key.as_str())
            .map(|held| {
                held.iter()
                    .filter(|touch| {
                        touch.occurred_at <= conversion.occurred_at
                            && conversion.occurred_at - touch.occurred_at <= question.lookback_ms
                    })
                    .copied()
                    .collect()
            })
            .unwrap_or_default();

        if journey.is_empty() {
            unattributed += 1;
            unattributed_value = unattributed_value
                .add(&conversion.value)
                .ok_or_else(too_much)?;
            continue;
        }

        let shares = weights(question.model, &journey, conversion.occurred_at, settings);
        let parts = conversion.value.split(&shares);
        let share_total: f64 = shares.iter().filter(|w| w.is_finite() && **w > 0.0).sum();

        for (touch, part) in journey.iter().zip(parts) {
            let at = touch.touch.dimension(question.dimension);
            let held = credited.entry(at).or_insert((Amount::ZERO, 0.0, 0, 0));
            held.0 = held.0.add(&part).ok_or_else(too_much)?;
            held.2 += 1;
        }
        // The conversion count each row takes is its share, so a linear result
        // over three touches reports a third of a conversion each rather than
        // three conversions. Whole conversions are `conversions`.
        for (index, touch) in journey.iter().enumerate() {
            let at = touch.touch.dimension(question.dimension);
            let share = if share_total > 0.0 {
                shares[index].max(0.0) / share_total
            } else {
                1.0 / journey.len() as f64
            };
            if let Some(held) = credited.get_mut(&at) {
                held.1 += share;
            }
        }

        // An assist is a touch that was inside the window and took no credit.
        // `docs/DATA_MODEL.md` section 6 calls these assisted conversions, and
        // they are what a single-touch model hides: the campaigns that were
        // part of the journey and earned nothing for it.
        //
        // It is counted from the **weight** rather than from the credited
        // amount. A share of a very small conversion can round away to nothing,
        // and having been given a share that rounded to zero is not the same as
        // having been given no share at all.
        for (index, touch) in journey.iter().enumerate() {
            if shares.get(index).copied().unwrap_or(0.0) > 0.0 {
                continue;
            }
            let at = touch.touch.dimension(question.dimension);
            if let Some(held) = credited.get_mut(&at) {
                held.3 += 1;
            }
        }
    }

    let mut answer = Attribution {
        model: question.model,
        model_version: MODEL_VERSION,
        settings_version: settings.version,
        rows: credited
            .into_iter()
            .map(
                |(dimension, (value, conversions, touches, assists))| Credit {
                    dimension,
                    value,
                    conversions,
                    touches,
                    assists,
                },
            )
            .collect(),
        total_value: total,
        conversions: conversions.len() as u64,
        unattributed,
        unattributed_value,
        folded_orders,
        without_consent,
        currency: (!mixed_currency).then_some(currency).flatten(),
        coverage,
    };
    // Largest credited value first, then by name, so a report reads in the
    // order somebody wants it and two runs of one query agree.
    answer.rows.sort_by(|a, b| {
        b.value
            .at_scale(a.value.scale.max(b.value.scale))
            .map(|v| v.units)
            .unwrap_or(b.value.units)
            .cmp(
                &a.value
                    .at_scale(a.value.scale.max(b.value.scale))
                    .map(|v| v.units)
                    .unwrap_or(a.value.units),
            )
            .then(a.dimension.cmp(&b.dimension))
    });
    Ok(answer)
}

fn too_much() -> TallyOwlError {
    TallyOwlError::new(
        ErrorCode::BudgetExceeded,
        "The conversion value over this range is larger than an exact total can hold. Narrow the range.",
    )
    .retryable(false)
}

// ---------------------------------------------------------------------------
// The campaign report
// ---------------------------------------------------------------------------

/// One row of a campaign report.
#[derive(Debug, Clone, PartialEq)]
pub struct CampaignRow {
    pub dimension: String,
    pub touches: u64,
    /// Distinct people who took part.
    pub people: u64,
    /// Distinct sessions the touches belonged to.
    pub sessions: u64,
    pub conversions: f64,
    /// Touches that were inside the window and took no credit under this model.
    pub assists: u64,
    pub value: Amount,
    pub cost: Amount,
    /// Credited value against cost. `None` when no cost was imported, because a
    /// return with no cost in it is a number that reads as free.
    pub return_on_spend: Option<f64>,
}

/// The whole campaign report.
#[derive(Debug, Clone, PartialEq)]
pub struct CampaignSummary {
    pub dimension: Dimension,
    pub model: Model,
    pub model_version: u64,
    pub settings_version: u64,
    pub rows: Vec<CampaignRow>,
    pub total_value: Amount,
    pub total_cost: Amount,
    pub currency: Option<String>,
    pub unattributed: u64,
    pub unattributed_value: Amount,
    pub coverage: Coverage,
}

/// Run one campaign report.
///
/// It runs the same attribution the `attribution` operator runs, with the same
/// model, the same window, and the same settings, so a summary row and an
/// attribution row can never disagree. It adds the people, the sessions, and
/// the imported cost, which is what makes it a report rather than a credit
/// list.
pub fn summarize(
    rows: &[EventRow],
    incomplete: bool,
    identity: &Identity,
    question: &Question,
    settings: &Settings,
) -> Result<CampaignSummary, TallyOwlError> {
    let attribution = run(rows, incomplete, identity, question, settings)?;

    // People and sessions for each dimension value, from the touches.
    let mut coverage = Coverage::default();
    let touches = touches(rows, identity, question, &mut coverage);
    let mut people: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    let mut sessions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let by_event: BTreeMap<[u8; 16], &EventRow> =
        rows.iter().map(|row| (row.event_id, row)).collect();
    for touch in &touches {
        let at = touch.touch.dimension(question.dimension);
        people.entry(at.clone()).or_default().insert(&touch.key);
        if let Some(session) = by_event
            .get(&touch.event_id)
            .and_then(|row| row.session_id.clone())
        {
            sessions.entry(at).or_default().insert(session);
        }
    }

    let cost = costs(rows, question.dimension);
    let mut total_cost = Amount::ZERO;
    for amount in cost.values() {
        total_cost = total_cost.add(amount).ok_or_else(too_much)?;
    }

    let mut named: BTreeSet<String> = attribution
        .rows
        .iter()
        .map(|row| row.dimension.clone())
        .collect();
    // A campaign that cost money and earned nothing is a row. Leaving it out
    // would make the report say every campaign paid for itself.
    named.extend(cost.keys().cloned());
    named.extend(people.keys().cloned());

    let credited: BTreeMap<&str, &Credit> = attribution
        .rows
        .iter()
        .map(|row| (row.dimension.as_str(), row))
        .collect();

    let mut out: Vec<CampaignRow> = named
        .into_iter()
        .map(|dimension| {
            let credit = credited.get(dimension.as_str());
            let value = credit.map(|c| c.value).unwrap_or(Amount::ZERO);
            let spend = cost.get(&dimension).copied().unwrap_or(Amount::ZERO);
            CampaignRow {
                touches: credit.map(|c| c.touches).unwrap_or(0),
                people: people.get(&dimension).map(BTreeSet::len).unwrap_or(0) as u64,
                sessions: sessions.get(&dimension).map(BTreeSet::len).unwrap_or(0) as u64,
                conversions: credit.map(|c| c.conversions).unwrap_or(0.0),
                assists: credit.map(|c| c.assists).unwrap_or(0),
                value,
                cost: spend,
                return_on_spend: ratio(&value, &spend),
                dimension,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.conversions
            .partial_cmp(&a.conversions)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.dimension.cmp(&b.dimension))
    });

    Ok(CampaignSummary {
        dimension: question.dimension,
        model: attribution.model,
        model_version: attribution.model_version,
        settings_version: attribution.settings_version,
        rows: out,
        total_value: attribution.total_value,
        total_cost,
        currency: attribution.currency,
        unattributed: attribution.unattributed,
        unattributed_value: attribution.unattributed_value,
        coverage: attribution.coverage,
    })
}

/// The imported cost for each dimension value.
///
/// A cost record names a campaign and a period. Only the campaign dimension can
/// hold one: a cost import says what a campaign cost and does not say how that
/// spend divided between the channels the campaign reached. A report grouped by
/// channel therefore shows no cost rather than a made-up share of one.
fn costs(rows: &[EventRow], dimension: Dimension) -> BTreeMap<String, Amount> {
    let mut out: BTreeMap<String, Amount> = BTreeMap::new();
    if dimension != Dimension::Campaign {
        return out;
    }
    for row in rows {
        if row.kind != "campaign-cost" {
            continue;
        }
        let Some((PropertyValue::Decimal(text), _)) = row.properties.get("cost") else {
            continue;
        };
        let Some(amount) = Amount::parse(text) else {
            continue;
        };
        let campaign = text_of(row, "campaign").unwrap_or_else(|| campaign::UNSET.to_string());
        let held = out.entry(campaign).or_insert(Amount::ZERO);
        if let Some(sum) = held.add(&amount) {
            *held = sum;
        }
    }
    out
}

/// Credited value against cost, when there is a cost.
fn ratio(value: &Amount, cost: &Amount) -> Option<f64> {
    if cost.is_zero() {
        return None;
    }
    let scale = value.scale.max(cost.scale);
    let value = value.at_scale(scale)?.units as f64;
    let cost = cost.at_scale(scale)?.units as f64;
    Some(value / cost)
}

// ---------------------------------------------------------------------------
// The durable settings
// ---------------------------------------------------------------------------

/// A project's attribution settings, and where they live.
///
/// **They are durable**, in the control catalog beside the workspaces, the
/// keys, and the collection policy. L112 built that home for the same reason
/// this uses it: a change an operator made and a restart lost is a change the
/// operator believes is set. `docs/FAILURE_MODES.md` section 7 lists saved
/// control state among the things a catalog rebuild cannot restore.
///
/// The working copy stays in memory, because an attribution question asks for
/// it and a durable read for every question would put the catalog on the query
/// path. The two cannot disagree: a write goes to the catalog **first** and
/// reaches memory only when that returns.
pub struct AttributionService {
    store: Option<std::sync::Arc<tallyowl_store::SegmentedStore>>,
    held: std::sync::Mutex<BTreeMap<[u8; 16], Settings>>,
}

impl Default for AttributionService {
    fn default() -> AttributionService {
        AttributionService::new()
    }
}

impl AttributionService {
    /// A set that nothing outlives. For a test that is not about durability.
    pub fn new() -> AttributionService {
        AttributionService {
            store: None,
            held: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Every project's settings, read back from the catalog.
    ///
    /// A stored record that will not check is **skipped and named** rather than
    /// refused, so one unreadable record cannot stop a head starting. That
    /// project answers under the shipped defaults until somebody fixes it, and
    /// the caller logs what did not apply.
    pub fn open(
        store: std::sync::Arc<tallyowl_store::SegmentedStore>,
    ) -> (AttributionService, Vec<String>) {
        let mut held: BTreeMap<[u8; 16], Settings> = BTreeMap::new();
        let mut refused = Vec::new();
        match store.catalog().projects() {
            Err(e) => refused.push(format!(
                "The stored projects could not be read, so no attribution settings were loaded: {e}"
            )),
            Ok(projects) => {
                for project in projects {
                    match store.catalog().attribution(project.project_id) {
                        Err(e) => refused.push(format!(
                            "The attribution settings of `{}` could not be read: {e}",
                            project.name
                        )),
                        Ok(None) => {}
                        Ok(Some(record)) => {
                            let settings = from_record(&record);
                            match settings.check() {
                                Ok(()) => {
                                    held.insert(project.project_id, settings);
                                }
                                Err(e) => refused.push(format!(
                                    "The attribution settings of `{}` were not applied: {}",
                                    project.name, e.message
                                )),
                            }
                        }
                    }
                }
            }
        }
        (
            AttributionService {
                store: Some(store),
                held: std::sync::Mutex::new(held),
            },
            refused,
        )
    }

    /// What one project answers under. The shipped defaults when it never set
    /// any of its own.
    pub fn settings(&self, project_id: [u8; 16]) -> Settings {
        self.held
            .lock()
            .expect("attribution settings")
            .get(&project_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Store one project's settings.
    ///
    /// They check before they are stored. Settings that would refuse every
    /// question never replace settings that answer one.
    pub fn put(
        &self,
        project_id: [u8; 16],
        settings: Settings,
        who: &str,
    ) -> Result<Settings, TallyOwlError> {
        settings.check()?;
        let mut held = self.held.lock().expect("attribution settings");
        let mut stored = settings;
        if let Some(store) = &self.store {
            let version = store
                .catalog()
                .put_attribution(&to_record(project_id, &stored, who))
                .map_err(|e| {
                    TallyOwlError::unavailable(format!(
                        "These attribution settings were not stored, so they were not applied either: {e}"
                    ))
                })?;
            stored.version = version;
        } else {
            stored.version = held.get(&project_id).map(|s| s.version).unwrap_or(0) + 1;
        }
        held.insert(project_id, stored.clone());
        Ok(stored)
    }
}

fn to_record(
    project_id: [u8; 16],
    settings: &Settings,
    who: &str,
) -> tallyowl_store::control::AttributionRecord {
    tallyowl_store::control::AttributionRecord {
        project_id,
        position_first_weight: settings.position_first_weight,
        position_last_weight: settings.position_last_weight,
        decay_half_life_ms: settings.decay_half_life_ms,
        lookback_ms: settings.lookback_ms,
        enabled_models: settings.enabled_names(),
        touch_retention_ms: settings.touch_retention_ms,
        settings_version: settings.version,
        updated_at: tallyowl_obs::time::now_ms(),
        updated_by: who.to_string(),
    }
}

fn from_record(record: &tallyowl_store::control::AttributionRecord) -> Settings {
    Settings {
        position_first_weight: record.position_first_weight,
        position_last_weight: record.position_last_weight,
        decay_half_life_ms: record.decay_half_life_ms,
        lookback_ms: record.lookback_ms,
        enabled_models: record
            .enabled_models
            .iter()
            .filter_map(|name| Model::parse(name))
            .collect(),
        touch_retention_ms: record.touch_retention_ms,
        version: record.settings_version,
    }
}

/// One project's settings, in the shape the contract carries.
pub fn to_wire(
    project_id: [u8; 16],
    settings: &Settings,
) -> tallyowl_control_api::types::AttributionSettings {
    tallyowl_control_api::types::AttributionSettings {
        project_id: project_id.to_vec(),
        position_first_weight: settings.position_first_weight,
        position_last_weight: settings.position_last_weight,
        decay_half_life_ms: settings.decay_half_life_ms,
        lookback_ms: settings.lookback_ms,
        enabled_models: settings.enabled_models.iter().map(to_wire_model).collect(),
        touch_retention_ms: settings.touch_retention_ms,
        settings_version: Some(settings.version),
        updated_at: None,
        updated_by: None,
    }
}

pub fn from_wire(settings: &tallyowl_control_api::types::AttributionSettings) -> Settings {
    Settings {
        position_first_weight: settings.position_first_weight,
        position_last_weight: settings.position_last_weight,
        decay_half_life_ms: settings.decay_half_life_ms,
        lookback_ms: settings.lookback_ms,
        enabled_models: settings
            .enabled_models
            .iter()
            .map(from_wire_model)
            .collect(),
        touch_retention_ms: settings.touch_retention_ms,
        // The version is the catalog's, never the caller's. A caller that could
        // set it could make two different answers claim the same version.
        version: 0,
    }
}

pub fn to_wire_model(model: &Model) -> tallyowl_control_api::types::AttributionModel {
    use tallyowl_control_api::types::AttributionModel as W;
    match model {
        Model::FirstTouch => W::FirstTouch,
        Model::LastTouch => W::LastTouch,
        Model::LastNonDirect => W::LastNonDirect,
        Model::Linear => W::Linear,
        Model::Position => W::Position,
        Model::Decay => W::Decay,
    }
}

pub fn from_wire_model(model: &tallyowl_control_api::types::AttributionModel) -> Model {
    use tallyowl_control_api::types::AttributionModel as W;
    match model {
        W::FirstTouch => Model::FirstTouch,
        W::LastTouch => Model::LastTouch,
        W::LastNonDirect => Model::LastNonDirect,
        W::Linear => Model::Linear,
        W::Position => Model::Position,
        W::Decay => Model::Decay,
    }
}

#[cfg(test)]
mod tests;
