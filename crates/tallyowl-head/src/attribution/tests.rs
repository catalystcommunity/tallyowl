//! Attribution fixtures.
//!
//! **Every fixture here is one a person can work out by hand.** The Phase 9
//! exit criterion is "first, last, non-direct, linear, position, and decay
//! fixtures are stable", and a fixture is only stable if somebody can say what
//! the answer should be before they read what it was. Each test names the
//! answer and the reason in its comment, and then asserts it.
//!
//! The journey nearly every test uses is the same one, so the six models can be
//! compared against each other:
//!
//! ```text
//!   day 0   a paid search click        (google / cpc / spring)
//!   day 3   a link from a partner site (partner.example)
//!   day 9   the person types the address (direct)
//!   day 10  they buy, for 100
//! ```
//!
//! One person, three touches, one conversion, and a round number so the
//! arithmetic is visible.

use super::*;
use crate::identity::{Identity, Resolution};
use tallyowl_store::row::EventRow;

const DAY: i64 = 86_400_000;
const PROJECT: [u8; 16] = [7; 16];
const PERSON: &str = "person-1";

fn id(byte: u8) -> [u8; 16] {
    [byte; 16]
}

fn row(byte: u8, kind: &str, name: &str, at: i64) -> EventRow {
    let mut row = EventRow::new(id(byte), kind, name, at);
    row.project_id = PROJECT;
    row.with_property(
        crate::identity::END_USER_ID,
        PropertyValue::Text(PERSON.to_string()),
        "client",
    )
}

/// One campaign touch, with the parameters a landing page carried.
fn touch(byte: u8, at: i64, source: &str, medium: Option<&str>, campaign: &str) -> EventRow {
    let mut held = row(byte, "campaign-touch", campaign, at)
        .with_property(
            "campaign_source",
            PropertyValue::Text(source.to_string()),
            "client",
        )
        .with_property(
            "campaign",
            PropertyValue::Text(campaign.to_string()),
            "client",
        );
    if let Some(medium) = medium {
        held = held.with_property(
            "campaign_medium",
            PropertyValue::Text(medium.to_string()),
            "client",
        );
    }
    held
}

/// A direct touch: somebody opened the site with no referrer and no campaign.
/// It is a page view rather than a campaign touch, because that is what a client
/// actually sends, and the classifier is what decides it is direct.
fn direct(byte: u8, at: i64) -> EventRow {
    row(byte, "campaign-touch", "(direct)", at)
}

fn purchase(byte: u8, at: i64, value: &str) -> EventRow {
    row(byte, "conversion", "purchase", at)
        .with_property("value", PropertyValue::Decimal(value.to_string()), "client")
        .with_property("goal", PropertyValue::Text("purchase".into()), "client")
        .with_property("currency", PropertyValue::Text("USD".into()), "client")
}

/// The journey the module note draws.
fn journey() -> Vec<EventRow> {
    vec![
        touch(1, 0, "google", Some("cpc"), "spring"),
        touch(2, 3 * DAY, "partner.example", None, "partner"),
        direct(3, 9 * DAY),
        purchase(4, 10 * DAY, "100"),
    ]
}

/// `campaign == <name>`, as a touch filter.
fn campaign_is(name: &str) -> Vec<u8> {
    use tallyowl_control_api::types::CompareOp;
    use tallyowl_wire::query;
    query::expression_ref(&query::compare(
        CompareOp::Eq,
        &query::expression::field(query::field("campaign")),
        &query::expression::literal(tallyowl_wire::control::write(&tallyowl_wire::Value::Text(
            name.to_string(),
        ))),
    ))
}

fn ask(model: Model) -> Question {
    Question {
        goal: "purchase".to_string(),
        model,
        lookback_ms: 30 * DAY,
        dimension: Dimension::Campaign,
        touch_filter: None,
        needs_consent: false,
        resolution: Resolution::LatestKnown,
    }
}

fn answer(rows: &[EventRow], question: &Question) -> Attribution {
    let identity = Identity::build(PROJECT, rows);
    run(rows, false, &identity, question, &Settings::default()).expect("the question is answerable")
}

/// The credited value for one dimension value, as text.
fn credited(answer: &Attribution, dimension: &str) -> String {
    answer
        .rows
        .iter()
        .find(|row| row.dimension == dimension)
        .map(|row| row.value.to_text())
        .unwrap_or_else(|| format!("no row for `{dimension}` in {:?}", answer.rows))
}

// ---------------------------------------------------------------------------
// The six models, over one journey
// ---------------------------------------------------------------------------

#[test]
fn first_touch_credits_the_paid_click_that_started_it() {
    // The first touch is the paid search click on day 0. It takes all 100 and
    // the other two take nothing.
    let answer = answer(&journey(), &ask(Model::FirstTouch));
    assert_eq!(credited(&answer, "spring"), "100");
    assert_eq!(credited(&answer, "partner"), "0");
    assert_eq!(answer.conversions, 1);
    assert_eq!(answer.model_version, MODEL_VERSION);
}

#[test]
fn last_touch_credits_the_address_bar() {
    // The last touch is the direct visit on day 9. It takes all 100, and the
    // campaign that found the customer takes nothing. This is the result the
    // non-direct model exists to argue with, and both are right about their own
    // question.
    let answer = answer(&journey(), &ask(Model::LastTouch));
    assert_eq!(credited(&answer, campaign::UNSET), "100");
    assert_eq!(credited(&answer, "spring"), "0");
}

#[test]
fn last_non_direct_skips_the_address_bar_and_credits_the_partner() {
    // The last touch that was not direct is the partner link on day 3. It takes
    // all 100.
    let answer = answer(&journey(), &ask(Model::LastNonDirect));
    assert_eq!(credited(&answer, "partner"), "100");
    assert_eq!(credited(&answer, campaign::UNSET), "0");
}

#[test]
fn a_journey_that_is_direct_all_the_way_through_still_credits_something() {
    // Nothing in this journey is a campaign, so the non-direct rule has nothing
    // to skip to. It falls back to the last touch rather than crediting
    // nothing, because crediting nothing would take the revenue out of the
    // report and the revenue happened.
    let rows = vec![direct(1, 0), direct(2, DAY), purchase(3, 2 * DAY, "50")];
    let answer = answer(&rows, &ask(Model::LastNonDirect));
    assert_eq!(credited(&answer, campaign::UNSET), "50");
    assert_eq!(answer.unattributed, 0);
}

#[test]
fn linear_divides_the_hundred_three_ways_and_loses_nothing() {
    // Three touches, so each takes a third of 100. A third of 100 is not exact,
    // so the assertion is the one that matters: the three parts add back up to
    // 100 exactly, and no two differ by more than the smallest unit.
    let answer = answer(&journey(), &ask(Model::Linear));
    assert_eq!(answer.rows.len(), 3);
    assert_eq!(answer.accounted().to_text(), "100");

    let mut parts: Vec<String> = answer.rows.iter().map(|row| row.value.to_text()).collect();
    parts.sort();
    // 100 / 3 at six guard digits is 33.333333, and one part carries the unit
    // that did not divide.
    assert_eq!(
        parts,
        vec![
            "33.333333".to_string(),
            "33.333333".to_string(),
            "33.333334".to_string()
        ]
    );
}

#[test]
fn position_gives_the_ends_forty_each_and_the_middle_twenty() {
    // The shipped defaults are 0.4 to the first and 0.4 to the last, so the one
    // touch in the middle takes the remaining 0.2. Over 100 that is exactly 40,
    // 20, and 40.
    let answer = answer(&journey(), &ask(Model::Position));
    assert_eq!(credited(&answer, "spring"), "40");
    assert_eq!(credited(&answer, "partner"), "20");
    assert_eq!(credited(&answer, campaign::UNSET), "40");
    assert_eq!(answer.accounted().to_text(), "100");
}

#[test]
fn position_over_two_touches_shares_what_the_two_ends_were_given() {
    // There is no middle to hold the remaining 0.2, and an operator who set 40
    // and 40 meant the two ends equally. 50 and 50.
    let rows = vec![
        touch(1, 0, "google", Some("cpc"), "spring"),
        touch(2, DAY, "partner.example", None, "partner"),
        purchase(3, 2 * DAY, "100"),
    ];
    let answer = answer(&rows, &ask(Model::Position));
    assert_eq!(credited(&answer, "spring"), "50");
    assert_eq!(credited(&answer, "partner"), "50");
}

#[test]
fn position_over_one_touch_gives_it_everything() {
    let rows = vec![
        touch(1, 0, "google", Some("cpc"), "spring"),
        purchase(2, DAY, "100"),
    ];
    let answer = answer(&rows, &ask(Model::Position));
    assert_eq!(credited(&answer, "spring"), "100");
}

#[test]
fn decay_halves_the_credit_for_every_week_before_the_purchase() {
    // The half-life default is seven days. Against a purchase on day 14:
    //   day 14 → age 0 weeks  → weight 1
    //   day  7 → age 1 week   → weight 0.5
    //   day  0 → age 2 weeks  → weight 0.25
    // The shares are 1 : 0.5 : 0.25, which over a total of 1.75 is
    // 4/7 : 2/7 : 1/7. Over 700 that is 400, 200, and 100.
    let rows = vec![
        touch(1, 0, "google", Some("cpc"), "early"),
        touch(2, 7 * DAY, "partner.example", None, "middle"),
        touch(3, 14 * DAY, "news.example", None, "late"),
        purchase(4, 14 * DAY, "700"),
    ];
    let answer = answer(&rows, &ask(Model::Decay));
    assert_eq!(credited(&answer, "late"), "400");
    assert_eq!(credited(&answer, "middle"), "200");
    assert_eq!(credited(&answer, "early"), "100");
    assert_eq!(answer.accounted().to_text(), "700");
}

#[test]
fn every_model_credits_the_whole_value_and_never_more() {
    // The property that has to hold whatever the model is: what the rows add up
    // to, plus what nothing earned, is the revenue that arrived. A model that
    // broke this would produce a campaign report that disagreed with the
    // revenue report and nobody could say which was right.
    for model in Model::all() {
        let answer = answer(&journey(), &ask(model));
        assert_eq!(
            answer.accounted().to_text(),
            "100",
            "the `{}` model did not add up",
            model.as_str()
        );
        assert_eq!(answer.total_value.to_text(), "100");
    }
}

// ---------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------

#[test]
fn a_touch_outside_the_window_earns_nothing_and_the_conversion_is_still_counted() {
    // The touch is 40 days before the purchase and the window is 30. Nothing
    // earned this conversion, so it is unattributed: its value is reported and
    // it is credited to no campaign. Crediting it to the touch outside the
    // window would be the defect D40 describes.
    let rows = vec![
        touch(1, 0, "google", Some("cpc"), "spring"),
        purchase(2, 40 * DAY, "100"),
    ];
    let answer = answer(&rows, &ask(Model::LastTouch));
    assert_eq!(answer.conversions, 1);
    assert_eq!(answer.unattributed, 1);
    assert_eq!(answer.unattributed_value.to_text(), "100");
    assert_eq!(answer.accounted().to_text(), "100");
}

#[test]
fn a_touch_after_the_conversion_never_earns_it() {
    // Somebody clicked an advertisement the day after they bought. The purchase
    // was already made, and an attribution that looked forwards would credit an
    // advertisement for a sale it could not have caused.
    let rows = vec![
        purchase(1, 10 * DAY, "100"),
        touch(2, 11 * DAY, "google", Some("cpc"), "spring"),
    ];
    let answer = answer(&rows, &ask(Model::LastTouch));
    assert_eq!(answer.unattributed, 1);
}

#[test]
fn a_window_longer_than_the_retained_range_is_refused_and_both_numbers_are_named() {
    // D40 and POLICY.md section 5. The result would look correct and be wrong,
    // so it is not returned at all.
    let identity = Identity::build(PROJECT, &[]);
    let mut question = ask(Model::LastTouch);
    question.lookback_ms = 200 * DAY;
    let refusal = run(&[], false, &identity, &question, &Settings::default())
        .expect_err("a window past the retained range is refused");
    assert_eq!(refusal.code, ErrorCode::FailedPrecondition);
    assert!(refusal.message.contains(&(200 * DAY).to_string()));
    assert!(refusal.message.contains(&(90 * DAY).to_string()));
}

#[test]
fn a_model_the_project_does_not_answer_is_refused_by_name() {
    let identity = Identity::build(PROJECT, &[]);
    let settings = Settings {
        enabled_models: [Model::LastTouch].into_iter().collect(),
        ..Settings::default()
    };
    let refusal = run(&[], false, &identity, &ask(Model::Decay), &settings)
        .expect_err("a model that is not enabled is refused");
    assert_eq!(refusal.code, ErrorCode::FailedPrecondition);
    assert!(refusal.message.contains("decay"));
    assert!(refusal.message.contains("last-touch"));
}

// ---------------------------------------------------------------------------
// A model change recomputes, and rewrites nothing
// ---------------------------------------------------------------------------

#[test]
fn changing_the_weights_changes_the_answer_and_not_one_stored_row() {
    // The exit criterion "model changes recompute from immutable facts", as a
    // test: the same rows answered twice under two settings give two answers,
    // and the rows are byte for byte what they were.
    let rows = journey();
    let before = rows.clone();

    let even = Settings {
        position_first_weight: 1.0 / 3.0,
        position_last_weight: 1.0 / 3.0,
        version: 1,
        ..Settings::default()
    };
    let ends = Settings {
        position_first_weight: 0.5,
        position_last_weight: 0.5,
        version: 2,
        ..Settings::default()
    };

    let identity = Identity::build(PROJECT, &rows);
    let first = run(&rows, false, &identity, &ask(Model::Position), &even).unwrap();
    let second = run(&rows, false, &identity, &ask(Model::Position), &ends).unwrap();

    // Even thirds against nothing for the middle.
    assert_eq!(credited(&first, "partner"), "33.333334");
    assert_eq!(credited(&second, "partner"), "0");
    assert_eq!(credited(&second, "spring"), "50");

    // The version travels, so the two answers cannot be mistaken for one.
    assert_eq!(first.settings_version, 1);
    assert_eq!(second.settings_version, 2);

    assert_eq!(rows, before, "attribution rewrote a stored row");
}

// ---------------------------------------------------------------------------
// Idempotent conversions and orders
// ---------------------------------------------------------------------------

#[test]
fn one_order_delivered_three_times_is_one_conversion_and_one_value() {
    // A checkout retried, a webhook arrived twice, and the person refreshed the
    // receipt page. Three rows, three different event identifiers, one order.
    let mut rows = vec![touch(1, 0, "google", Some("cpc"), "spring")];
    for (index, at) in [(10u8, 5 * DAY), (11, 5 * DAY + 200), (12, 6 * DAY)] {
        rows.push(purchase(index, at, "19.99").with_property(
            "order_id",
            PropertyValue::Text("order-77".into()),
            "client",
        ));
    }
    let answer = answer(&rows, &ask(Model::LastTouch));
    assert_eq!(answer.conversions, 1);
    assert_eq!(answer.folded_orders, 2);
    assert_eq!(answer.total_value.to_text(), "19.99");
    assert_eq!(credited(&answer, "spring"), "19.99");
}

#[test]
fn a_repeat_that_arrived_late_cannot_widen_the_window() {
    // The earliest row of an order wins. The touch is 20 days before the first
    // delivery and 40 days before the late repeat, and the window is 30. If the
    // repeat decided the conversion time, the touch would fall outside the
    // window and a campaign would lose credit it earned.
    let rows = vec![
        touch(1, 0, "google", Some("cpc"), "spring"),
        purchase(10, 20 * DAY, "100").with_property(
            "order_id",
            PropertyValue::Text("order-9".into()),
            "client",
        ),
        purchase(11, 40 * DAY, "100").with_property(
            "order_id",
            PropertyValue::Text("order-9".into()),
            "client",
        ),
    ];
    let answer = answer(&rows, &ask(Model::LastTouch));
    assert_eq!(answer.conversions, 1);
    assert_eq!(credited(&answer, "spring"), "100");
    assert_eq!(answer.unattributed, 0);
}

#[test]
fn two_conversions_with_no_order_are_two_conversions() {
    // Nothing says these are one, so they are two. A fold on value and time
    // would silently lose a genuine second purchase.
    let rows = vec![
        touch(1, 0, "google", Some("cpc"), "spring"),
        purchase(10, 5 * DAY, "19.99"),
        purchase(11, 6 * DAY, "19.99"),
    ];
    let answer = answer(&rows, &ask(Model::LastTouch));
    assert_eq!(answer.conversions, 2);
    assert_eq!(answer.folded_orders, 0);
    assert_eq!(credited(&answer, "spring"), "39.98");
}

// ---------------------------------------------------------------------------
// Consent
// ---------------------------------------------------------------------------

#[test]
fn a_person_who_denied_marketing_consent_is_left_out_when_the_policy_asks() {
    // D30. Two people buy for 100 each and one denied marketing consent. With
    // consent required the report holds one conversion and 100; without it,
    // two and 200. Nothing was deleted either way: the row is stored and the
    // policy decides whether attribution reads it.
    let mut rows = journey();
    // A second person, who denied.
    let mut denied: Vec<EventRow> = vec![
        touch(20, 0, "google", Some("cpc"), "spring"),
        purchase(21, 2 * DAY, "100"),
    ];
    for row in denied.iter_mut() {
        row.properties.insert(
            crate::identity::END_USER_ID.to_string(),
            (PropertyValue::Text("person-2".into()), "client".to_string()),
        );
        row.properties.insert(
            CONSENT_MARKETING.to_string(),
            (PropertyValue::Text("denied".into()), "client".to_string()),
        );
    }
    rows.extend(denied);

    let mut permissive = ask(Model::FirstTouch);
    permissive.needs_consent = false;
    let all = answer(&rows, &permissive);
    assert_eq!(all.conversions, 2);
    assert_eq!(all.total_value.to_text(), "200");
    assert_eq!(all.without_consent, 0);

    let mut strict = ask(Model::FirstTouch);
    strict.needs_consent = true;
    let consented = answer(&rows, &strict);
    assert_eq!(consented.conversions, 1);
    assert_eq!(consented.total_value.to_text(), "100");
    assert_eq!(consented.without_consent, 1);
}

#[test]
fn an_absent_consent_state_is_not_a_denial() {
    // TallyOwl does not guess. An application that never sent a consent state
    // has not said no on its person's behalf, and treating silence as a denial
    // would make every project that has not instrumented consent report zero
    // revenue the day the setting was turned on.
    let rows = journey();
    let mut strict = ask(Model::FirstTouch);
    strict.needs_consent = true;
    let answer = answer(&rows, &strict);
    assert_eq!(answer.conversions, 1);
    assert_eq!(answer.without_consent, 0);
}

// ---------------------------------------------------------------------------
// Touchpoints
// ---------------------------------------------------------------------------

#[test]
fn a_landing_page_view_is_a_touch_and_an_ordinary_page_view_is_not() {
    // A marketing landing page sends a page view with the campaign parameters
    // out of its address. A page view inside the application is somebody moving
    // around, and counting it as a touch would give every internal
    // navigation a share of the revenue.
    let landing = row(1, "page-view", "/spring", 0)
        .with_property("campaign", PropertyValue::Text("spring".into()), "client")
        .with_property(
            "campaign_source",
            PropertyValue::Text("google".into()),
            "client",
        );
    let inside = row(2, "page-view", "/pricing", DAY);
    let rows = vec![landing, inside, purchase(3, 2 * DAY, "100")];

    let answer = answer(&rows, &ask(Model::Linear));
    assert_eq!(answer.rows.len(), 1);
    assert_eq!(credited(&answer, "spring"), "100");
}

#[test]
fn an_explicit_touch_and_the_page_view_beside_it_are_one_landing() {
    // A client that sends both would otherwise be counted twice, and every
    // linear result from that client would be wrong by a factor nobody can see
    // from the report.
    let explicit = touch(1, 1_000, "google", Some("cpc"), "spring");
    let page = row(2, "page-view", "/spring", 1_400)
        .with_property("campaign", PropertyValue::Text("spring".into()), "client")
        .with_property(
            "campaign_source",
            PropertyValue::Text("google".into()),
            "client",
        )
        .with_property(
            "campaign_medium",
            PropertyValue::Text("cpc".into()),
            "client",
        );
    let rows = vec![explicit, page, purchase(3, DAY, "100")];

    let identity = Identity::build(PROJECT, &rows);
    let found = touches(
        &rows,
        &identity,
        &ask(Model::Linear),
        &mut Coverage::default(),
    );
    assert_eq!(found.len(), 1, "two rows for one landing: {found:?}");
}

#[test]
fn two_landings_far_enough_apart_are_two_touches() {
    // The same campaign, clicked twice, an hour apart. Those are two visits and
    // a fold that took any two matching rows would lose one.
    let rows = vec![
        touch(1, 0, "google", Some("cpc"), "spring"),
        touch(2, 3_600_000, "google", Some("cpc"), "spring"),
        purchase(3, DAY, "100"),
    ];
    let identity = Identity::build(PROJECT, &rows);
    let found = touches(
        &rows,
        &identity,
        &ask(Model::Linear),
        &mut Coverage::default(),
    );
    assert_eq!(found.len(), 2);
}

#[test]
fn a_touch_filter_narrows_the_journey_and_the_remaining_credit_still_adds_up() {
    let rows = journey();
    let identity = Identity::build(PROJECT, &rows);
    let mut question = ask(Model::Linear);
    question.touch_filter = Some(
        crate::expr::prepare_unchecked(&campaign_is("spring"), 16).expect("the filter compiles"),
    );

    let answer = run(&rows, false, &identity, &question, &Settings::default()).unwrap();
    assert_eq!(answer.rows.len(), 1);
    assert_eq!(credited(&answer, "spring"), "100");
    assert_eq!(answer.accounted().to_text(), "100");
}

// ---------------------------------------------------------------------------
// Breakdown dimensions
// ---------------------------------------------------------------------------

#[test]
fn a_breakdown_by_channel_reads_the_classifier_rather_than_a_stored_column() {
    // The rows carry no `campaign_channel` property at all, and the answer is
    // still grouped by channel, because the classifier runs at read time. That
    // is what makes a classifier change recompute.
    let mut question = ask(Model::Linear);
    question.dimension = Dimension::Channel;
    let answer = answer(&journey(), &question);

    let named: Vec<&str> = answer.rows.iter().map(|r| r.dimension.as_str()).collect();
    assert!(named.contains(&"paid-search"), "{named:?}");
    assert!(named.contains(&"referral"), "{named:?}");
    assert!(named.contains(&"direct"), "{named:?}");
}

// ---------------------------------------------------------------------------
// The campaign report
// ---------------------------------------------------------------------------

#[test]
fn a_campaign_report_holds_the_people_the_value_the_cost_and_the_return() {
    // One campaign, one person, 100 of revenue, and 25 of imported cost. The
    // return is 4.
    let mut rows = vec![
        touch(1, 0, "google", Some("cpc"), "spring"),
        purchase(2, DAY, "100"),
    ];
    let mut cost = EventRow::new(id(3), "campaign-cost", "spring", 0);
    cost.project_id = PROJECT;
    rows.push(
        cost.with_property("campaign", PropertyValue::Text("spring".into()), "client")
            .with_property("cost", PropertyValue::Decimal("25".into()), "client")
            .with_property("currency", PropertyValue::Text("USD".into()), "client"),
    );

    let identity = Identity::build(PROJECT, &rows);
    let report = summarize(
        &rows,
        false,
        &identity,
        &ask(Model::LastTouch),
        &Settings::default(),
    )
    .unwrap();

    let spring = report
        .rows
        .iter()
        .find(|row| row.dimension == "spring")
        .expect("a row for the campaign");
    assert_eq!(spring.value.to_text(), "100");
    assert_eq!(spring.cost.to_text(), "25");
    assert_eq!(spring.people, 1);
    assert_eq!(spring.return_on_spend, Some(4.0));
    assert_eq!(report.total_cost.to_text(), "25");
}

#[test]
fn a_campaign_that_cost_money_and_earned_nothing_is_still_a_row() {
    // Leaving it out would make the report say every campaign paid for itself,
    // which is the one thing a person reads a campaign report to find out.
    let mut cost = EventRow::new(id(3), "campaign-cost", "autumn", 0);
    cost.project_id = PROJECT;
    let rows = vec![cost
        .with_property("campaign", PropertyValue::Text("autumn".into()), "client")
        .with_property("cost", PropertyValue::Decimal("40".into()), "client")];

    let identity = Identity::build(PROJECT, &rows);
    let report = summarize(
        &rows,
        false,
        &identity,
        &ask(Model::LastTouch),
        &Settings::default(),
    )
    .unwrap();

    let autumn = report
        .rows
        .iter()
        .find(|row| row.dimension == "autumn")
        .expect("a row for the campaign that earned nothing");
    assert_eq!(autumn.value.to_text(), "0");
    assert_eq!(autumn.cost.to_text(), "40");
    assert_eq!(autumn.return_on_spend, Some(0.0));
}

#[test]
fn a_report_grouped_by_channel_shows_no_cost_rather_than_a_made_up_share() {
    // A cost import says what a campaign cost. It does not say how that spend
    // divided between the channels the campaign reached, and dividing it evenly
    // would be TallyOwl inventing the number a person is reading the report to
    // find out.
    let mut cost = EventRow::new(id(3), "campaign-cost", "spring", 0);
    cost.project_id = PROJECT;
    let mut rows = journey();
    rows.push(
        cost.with_property("campaign", PropertyValue::Text("spring".into()), "client")
            .with_property("cost", PropertyValue::Decimal("25".into()), "client"),
    );

    let identity = Identity::build(PROJECT, &rows);
    let mut question = ask(Model::LastTouch);
    question.dimension = Dimension::Channel;
    let report = summarize(&rows, false, &identity, &question, &Settings::default()).unwrap();

    assert_eq!(report.total_cost.to_text(), "0");
    assert!(report.rows.iter().all(|row| row.return_on_spend.is_none()));
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[test]
fn settings_that_would_refuse_every_question_are_refused_first() {
    let no_models = Settings {
        enabled_models: BTreeSet::new(),
        ..Settings::default()
    };
    assert!(no_models.check().is_err());

    let ends_over_one = Settings {
        position_first_weight: 0.7,
        position_last_weight: 0.7,
        ..Settings::default()
    };
    let refusal = ends_over_one
        .check()
        .expect_err("weights over 1 are refused");
    assert!(refusal.message.contains("nothing left"));

    let past_retention = Settings {
        lookback_ms: 120 * DAY,
        touch_retention_ms: 90 * DAY,
        ..Settings::default()
    };
    assert!(past_retention.check().is_err());

    let no_half_life = Settings {
        decay_half_life_ms: 0,
        ..Settings::default()
    };
    assert!(no_half_life.check().is_err());
}

#[test]
fn the_shipped_defaults_are_a_settings_set_that_applies() {
    Settings::default()
        .check()
        .expect("the shipped defaults have to be usable");
}

// ---------------------------------------------------------------------------
// Cross-project isolation
// ---------------------------------------------------------------------------

#[test]
fn a_touch_in_another_project_never_earns_this_project_s_conversion() {
    // The identity graph is built for one project and ignores every row that
    // does not carry it. Two projects that both use `person-1` are two people,
    // and this is the attribution half of that.
    let mut stranger = touch(9, 0, "google", Some("cpc"), "other-project-campaign");
    stranger.project_id = [8; 16];

    let rows = vec![stranger, purchase(2, DAY, "100")];
    let answer = answer(&rows, &ask(Model::LastTouch));
    // Nothing this project can see earned it.
    assert_eq!(answer.unattributed, 1);
    assert!(answer
        .rows
        .iter()
        .all(|row| row.dimension != "other-project-campaign"));
}

// ---------------------------------------------------------------------------
// Assisted conversions, and event-time identity
// ---------------------------------------------------------------------------

#[test]
fn a_single_touch_model_reports_what_it_did_not_pay_for() {
    // An assist is a touch that was inside the window and took no credit. It is
    // what a single-touch model hides: under first-touch the partner link and
    // the direct visit were both part of the journey and earned nothing, and a
    // report without that column says they were not there at all.
    let answer = answer(&journey(), &ask(Model::FirstTouch));

    let assists = |dimension: &str| {
        answer
            .rows
            .iter()
            .find(|row| row.dimension == dimension)
            .map(|row| row.assists)
            .unwrap_or_else(|| panic!("a row for `{dimension}`"))
    };
    assert_eq!(assists("spring"), 0, "the credited touch is not an assist");
    assert_eq!(assists("partner"), 1);
    assert_eq!(assists(campaign::UNSET), 1);
}

#[test]
fn a_model_that_credits_every_touch_reports_no_assists() {
    // Under linear every touch takes a share, so nothing assisted: it all
    // earned. A count that came from "credited value is zero" rather than from
    // the weight would report assists here as soon as a conversion was small
    // enough for a share to round away.
    let answer = answer(&journey(), &ask(Model::Linear));
    assert!(
        answer.rows.iter().all(|row| row.assists == 0),
        "{:?}",
        answer.rows
    );
}

#[test]
fn event_time_identity_leaves_the_anonymous_clicks_where_they_happened() {
    // The two resolutions answer different questions and this is the fixture
    // that separates them. One person clicks a campaign anonymously, signs in,
    // and buys.
    //
    //   - **latest known** says the buyer is the clicker, so the campaign is
    //     credited. That is what a conversion report means;
    //   - **event time** says the clicker was an anonymous visitor who is not
    //     the person who bought, so nothing joins and the conversion is
    //     unattributed. That is what a cohort question means.
    let anonymous = "anon-7";
    let mut touch_row = touch(1, 0, "google", Some("cpc"), "spring");
    touch_row.properties.remove(crate::identity::END_USER_ID);
    touch_row.properties.insert(
        crate::identity::ANONYMOUS_ID.to_string(),
        (PropertyValue::Text(anonymous.into()), "client".to_string()),
    );

    let mut identify = row(2, "identify", "identify", 5 * DAY);
    identify.properties.insert(
        crate::identity::ANONYMOUS_ID.to_string(),
        (PropertyValue::Text(anonymous.into()), "client".to_string()),
    );

    let rows = vec![touch_row, identify, purchase(3, 6 * DAY, "100")];

    let mut latest = ask(Model::FirstTouch);
    latest.resolution = Resolution::LatestKnown;
    let joined = answer(&rows, &latest);
    assert_eq!(credited(&joined, "spring"), "100");
    assert_eq!(joined.unattributed, 0);

    let mut at_the_time = ask(Model::FirstTouch);
    at_the_time.resolution = Resolution::EventTime;
    let separate = answer(&rows, &at_the_time);
    assert_eq!(separate.conversions, 1);
    assert_eq!(
        separate.unattributed, 1,
        "at event time the click belonged to somebody who was not yet the buyer"
    );
}
