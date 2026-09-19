//! Campaign and referrer capture, and the channel a touch belongs to.
//!
//! `docs/DATA_MODEL.md` section 3.6 says a touch records "channel
//! classification and classifier version". This module is that classifier.
//!
//! # It is derived, and it is derived at read time
//!
//! `AGENTS.md`: "Make all derived projections and rollups reproducible from
//! retained raw data." The channel is computed from the referring domain and
//! the campaign parameters every time somebody asks, rather than stored once
//! when the row arrived.
//!
//! That is deliberate and it costs a little work on every read. The reason is
//! the Phase 9 exit criterion **"model changes recompute from immutable
//! facts"**. A stored channel is a fact about the classifier that ran, not
//! about the traffic, so a corrected classifier would disagree with every row
//! written before it and the only fix would be a rewrite. Reclassifying at read
//! time makes a classifier change take effect on the next question and leaves
//! every stored byte alone.
//!
//! The row still carries `campaign_channel` and `campaign_classifier_version`,
//! because a general aggregate groups by a stored column and a person reading
//! one touch wants to see what it was called. Nothing that decides credit reads
//! those: [`classify`] runs again. Where the two differ, the row's version says
//! which classifier wrote it, so the difference is visible rather than silent.
//!
//! # Direct is not a channel a producer can claim
//!
//! A touch with no referrer and no campaign is direct. Everything else is
//! classified from what arrived. A producer cannot send a channel, because a
//! producer that could name its own channel could put paid traffic in the
//! organic column, and the number that decides a marketing budget would be one
//! the marketing team wrote.

use std::collections::BTreeMap;

use tallyowl_store::row::{EventRow, PropertyValue};

/// Which classifier produced a channel.
///
/// It rises when a rule changes. A result names it, so two answers computed
/// under different rules are never mistaken for one another.
pub const CLASSIFIER_VERSION: u64 = 1;

/// The property that holds the classified channel.
pub const CHANNEL: &str = "campaign_channel";
/// The property that holds the version of the classifier that wrote it.
pub const CLASSIFIER: &str = "campaign_classifier_version";
/// The property that holds the host the visit came from.
pub const REFERRER_DOMAIN: &str = "referrer_domain";
/// The property that holds the whole referring address.
pub const REFERRER: &str = "referrer";

/// The channel a touch belongs to.
///
/// The set is closed. An unrecognised combination becomes [`Channel::Other`]
/// rather than a new name, because a channel list that grows from the data is a
/// channel list nobody can total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Channel {
    /// No referrer and no campaign. Somebody typed the address or used a
    /// bookmark, as far as anything can tell.
    Direct,
    /// A search engine, without a paid marker.
    OrganicSearch,
    /// A search engine, with a paid marker.
    PaidSearch,
    /// A social network, without a paid marker.
    Social,
    /// A social network, with a paid marker.
    PaidSocial,
    Email,
    Display,
    Affiliate,
    /// Another site linked here.
    Referral,
    /// A campaign said something this build does not recognise.
    Other,
}

impl Channel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Channel::Direct => "direct",
            Channel::OrganicSearch => "organic-search",
            Channel::PaidSearch => "paid-search",
            Channel::Social => "social",
            Channel::PaidSocial => "paid-social",
            Channel::Email => "email",
            Channel::Display => "display",
            Channel::Affiliate => "affiliate",
            Channel::Referral => "referral",
            Channel::Other => "other",
        }
    }

    /// Whether a touch of this channel can take credit under the non-direct
    /// rule.
    ///
    /// `last-non-direct` exists because a person who found a product through a
    /// campaign, went away, and came back by typing the address should not have
    /// the campaign's work credited to the address bar. Direct is the only
    /// channel that rule skips.
    pub fn is_direct(&self) -> bool {
        matches!(self, Channel::Direct)
    }
}

/// What a classification is made from. Every field comes off a stored row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Touch {
    pub source: Option<String>,
    pub medium: Option<String>,
    pub campaign: Option<String>,
    pub content: Option<String>,
    pub term: Option<String>,
    pub click_id: Option<String>,
    pub referrer_domain: Option<String>,
}

impl Touch {
    /// Read one row's campaign fields.
    pub fn of(row: &EventRow) -> Touch {
        let text = |key: &str| match row.properties.get(key) {
            Some((PropertyValue::Text(value), _)) if !value.trim().is_empty() => {
                Some(value.trim().to_string())
            }
            _ => None,
        };
        Touch {
            source: text("campaign_source"),
            medium: text("campaign_medium"),
            campaign: text("campaign"),
            content: text("campaign_content"),
            term: text("campaign_term"),
            click_id: text("campaign_click_id"),
            // A row that carries only the whole address still classifies,
            // because the host is the part the rules read.
            referrer_domain: text(REFERRER_DOMAIN)
                .or_else(|| text(REFERRER).as_deref().and_then(host_of)),
        }
    }

    /// Whether this touch carries nothing at all.
    pub fn is_empty(&self) -> bool {
        !self.has_campaign() && self.referrer_domain.is_none()
    }

    /// Whether a campaign was tagged on this touch.
    ///
    /// **A referrer on its own is not a campaign**, and this is the difference
    /// that decides whether a page view is a touch. A browser sends a
    /// referrer for a link inside the application as readily as for one from
    /// outside it, so a rule that took any referrer would make every internal
    /// navigation a touch and give each one a share of the revenue. A
    /// referral that is genuinely a touch arrives as a `campaign-touch` row,
    /// which says so.
    pub fn has_campaign(&self) -> bool {
        self.source.is_some()
            || self.medium.is_some()
            || self.campaign.is_some()
            || self.click_id.is_some()
    }

    /// The value of one breakdown dimension.
    pub fn dimension(&self, dimension: Dimension) -> String {
        match dimension {
            Dimension::Campaign => self.campaign.clone(),
            Dimension::Channel => Some(classify(self).as_str().to_string()),
            Dimension::Source => self.source.clone(),
            Dimension::Medium => self.medium.clone(),
            Dimension::Content => self.content.clone(),
        }
        .unwrap_or_else(|| UNSET.to_string())
    }
}

/// What a row that carried nothing for a dimension is grouped under.
///
/// It is a name rather than an empty string, so a person reading a report can
/// tell "this traffic had no campaign" from "this cell is broken".
pub const UNSET: &str = "(none)";

/// Which touch field a campaign report groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Campaign,
    Channel,
    Source,
    Medium,
    Content,
}

impl Dimension {
    pub fn as_str(&self) -> &'static str {
        match self {
            Dimension::Campaign => "campaign",
            Dimension::Channel => "channel",
            Dimension::Source => "source",
            Dimension::Medium => "medium",
            Dimension::Content => "content",
        }
    }
}

/// The channel this touch belongs to.
///
/// The order of the rules is the whole of the behavior, so it is written here
/// rather than left to be read out of the code:
///
/// 1. **an explicit medium wins.** An operator who tagged a link `cpc` said
///    what it was, and a guess from the domain must not overrule them;
/// 2. **a click identifier means paid.** A platform that stamps one on a link
///    only does it for a click somebody paid for;
/// 3. **the referring domain decides the rest**, between search, social, and an
///    ordinary referral;
/// 4. **nothing at all is direct.**
pub fn classify(touch: &Touch) -> Channel {
    let medium = touch.medium.as_deref().map(str::to_ascii_lowercase);
    let source = touch.source.as_deref().map(str::to_ascii_lowercase);
    let domain = touch
        .referrer_domain
        .as_deref()
        .map(str::to_ascii_lowercase);

    // 1. An explicit medium wins.
    if let Some(medium) = medium.as_deref() {
        match medium {
            "cpc" | "ppc" | "paid" | "paidsearch" | "paid-search" | "sem" => {
                return Channel::PaidSearch
            }
            "paidsocial" | "paid-social" | "paid_social" => return Channel::PaidSocial,
            "email" | "newsletter" | "e-mail" => return Channel::Email,
            "display" | "banner" | "cpm" => return Channel::Display,
            "affiliate" | "partner" => return Channel::Affiliate,
            "organic" => return Channel::OrganicSearch,
            "social" | "social-network" | "social_network" => {
                // A social source with a paid marker beside it is paid social.
                // The marker is on the source rather than the medium when a
                // platform builds the link itself.
                return match touch.click_id {
                    Some(_) => Channel::PaidSocial,
                    None => Channel::Social,
                };
            }
            "referral" => return Channel::Referral,
            _ => {}
        }
    }

    // 2. A click identifier means somebody paid for the click.
    if touch.click_id.is_some() {
        let network = source.as_deref().is_some_and(is_social);
        return if network {
            Channel::PaidSocial
        } else {
            Channel::PaidSearch
        };
    }

    // 3. The referring domain, or the source when there is no referrer. A
    //    campaign that names `google` as its source and carries no medium is
    //    the same traffic as one that arrived from `google.com`.
    let host = domain.as_deref().or(source.as_deref());
    if let Some(host) = host {
        if is_search(host) {
            return Channel::OrganicSearch;
        }
        if is_social(host) {
            return Channel::Social;
        }
        if is_email(host) {
            return Channel::Email;
        }
        return Channel::Referral;
    }

    // 4. A campaign with no source, no medium, and no referrer said something
    //    this build cannot place. It is not direct: something was tagged.
    if touch.campaign.is_some() {
        return Channel::Other;
    }
    Channel::Direct
}

/// The classified channel of one stored row, computed rather than read.
pub fn channel_of(row: &EventRow) -> Channel {
    classify(&Touch::of(row))
}

/// The host part of an address.
///
/// It takes the whole address rather than parsing a URL, because a referrer is
/// whatever a browser sent and a strict parser would refuse values that carry
/// perfectly good hosts. A leading `www.` goes, so `www.example.com` and
/// `example.com` are one site rather than two rows in a report.
pub fn host_of(address: &str) -> Option<String> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return None;
    }
    let after_scheme = match trimmed.find("://") {
        Some(at) => &trimmed[at + 3..],
        None => trimmed,
    };
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Credentials and a port are not part of the site.
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    (!host.is_empty()).then_some(host)
}

/// The campaign properties a projection writes for one set of parameters.
///
/// The channel and the classifier version are here rather than left to the
/// query, so a general aggregate can group by channel. See the module note on
/// why nothing that decides credit reads them.
pub fn derived_properties(touch: &Touch) -> Vec<(String, PropertyValue)> {
    let channel = classify(touch);
    let mut out = vec![
        (
            CHANNEL.to_string(),
            PropertyValue::Text(channel.as_str().to_string()),
        ),
        (
            CLASSIFIER.to_string(),
            PropertyValue::Unsigned(CLASSIFIER_VERSION),
        ),
    ];
    if let Some(domain) = &touch.referrer_domain {
        out.push((
            REFERRER_DOMAIN.to_string(),
            PropertyValue::Text(domain.clone()),
        ));
    }
    out
}

/// How many touches each channel took, for a coverage report.
pub fn count_by_channel(rows: &[&EventRow]) -> BTreeMap<String, u64> {
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    for row in rows {
        *out.entry(channel_of(row).as_str().to_string()).or_default() += 1;
    }
    out
}

fn is_search(host: &str) -> bool {
    const ENGINES: &[&str] = &[
        "google",
        "bing",
        "duckduckgo",
        "yahoo",
        "yandex",
        "baidu",
        "ecosia",
        "brave",
        "startpage",
        "qwant",
        "searx",
    ];
    ENGINES.iter().any(|engine| matches_site(host, engine))
}

fn is_social(host: &str) -> bool {
    const NETWORKS: &[&str] = &[
        "facebook",
        "instagram",
        "linkedin",
        "reddit",
        "pinterest",
        "tiktok",
        "youtube",
        "mastodon",
        "bluesky",
        "bsky",
        "threads",
        "snapchat",
        "twitter",
        "x.com",
    ];
    NETWORKS.iter().any(|network| matches_site(host, network))
}

fn is_email(host: &str) -> bool {
    const PROVIDERS: &[&str] = &["mail", "outlook", "gmail", "webmail", "zoho"];
    PROVIDERS
        .iter()
        .any(|provider| matches_site(host, provider))
}

/// Whether a host belongs to a named site.
///
/// It matches the label rather than the whole string, so `google.co.uk` and
/// `news.google.com` are both Google and `notgoogleatall.example` is not. A
/// bare `contains` matched the third, which is the ordinary way a classifier
/// quietly puts a competitor's traffic in somebody else's column.
fn matches_site(host: &str, site: &str) -> bool {
    host.split('.').any(|label| label == site)
        || host == site
        || host.starts_with(&format!("{site}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch() -> Touch {
        Touch::default()
    }

    #[test]
    fn nothing_at_all_is_direct() {
        assert_eq!(classify(&touch()), Channel::Direct);
    }

    #[test]
    fn a_tagged_medium_beats_the_domain_it_arrived_from() {
        // Somebody paid Google for this click and tagged it. A guess from the
        // domain would call it organic and put paid traffic in the free column.
        let paid = Touch {
            medium: Some("cpc".into()),
            source: Some("google".into()),
            referrer_domain: Some("google.com".into()),
            ..touch()
        };
        assert_eq!(classify(&paid), Channel::PaidSearch);

        let free = Touch {
            referrer_domain: Some("google.com".into()),
            ..touch()
        };
        assert_eq!(classify(&free), Channel::OrganicSearch);
    }

    #[test]
    fn a_click_identifier_means_somebody_paid_for_the_click() {
        let search = Touch {
            click_id: Some("abc123".into()),
            source: Some("bing".into()),
            ..touch()
        };
        assert_eq!(classify(&search), Channel::PaidSearch);

        let social = Touch {
            click_id: Some("abc123".into()),
            source: Some("facebook".into()),
            ..touch()
        };
        assert_eq!(classify(&social), Channel::PaidSocial);
    }

    #[test]
    fn a_site_that_only_contains_an_engine_name_is_not_that_engine() {
        // The failure this exists for: a bare `contains` puts a competitor's
        // referral traffic in the organic-search column and nobody notices,
        // because organic search is the row nobody questions.
        let imposter = Touch {
            referrer_domain: Some("notgoogleatall.example".into()),
            ..touch()
        };
        assert_eq!(classify(&imposter), Channel::Referral);

        let real = Touch {
            referrer_domain: Some("news.google.com".into()),
            ..touch()
        };
        assert_eq!(classify(&real), Channel::OrganicSearch);
    }

    #[test]
    fn an_unknown_site_is_a_referral_and_an_unknown_tag_is_other() {
        let referral = Touch {
            referrer_domain: Some("partner.example".into()),
            ..touch()
        };
        assert_eq!(classify(&referral), Channel::Referral);

        // A campaign was tagged and named nothing this build can place. It is
        // not direct: something was tagged, and calling it direct would hide a
        // tagging mistake as ordinary traffic.
        let tagged = Touch {
            campaign: Some("spring".into()),
            ..touch()
        };
        assert_eq!(classify(&tagged), Channel::Other);
    }

    #[test]
    fn a_host_comes_out_of_an_address_without_its_scheme_port_or_leading_www() {
        assert_eq!(
            host_of("https://www.example.com:8443/landing?utm_source=x"),
            Some("example.com".to_string())
        );
        assert_eq!(host_of("example.com"), Some("example.com".to_string()));
        assert_eq!(host_of("   "), None);
        // A browser sends what it sends. A referrer with credentials in it
        // still has a host, and the credentials are not part of it.
        assert_eq!(
            host_of("http://user:secret@news.example.com/story"),
            Some("news.example.com".to_string())
        );
    }

    #[test]
    fn a_row_classifies_from_the_whole_address_when_no_domain_was_sent() {
        let row = EventRow::new([1; 16], "campaign-touch", "spring", 10).with_property(
            REFERRER,
            PropertyValue::Text("https://duckduckgo.com/?q=owls".into()),
            "client",
        );
        assert_eq!(channel_of(&row), Channel::OrganicSearch);
    }

    #[test]
    fn a_derived_property_set_names_the_classifier_that_wrote_it() {
        let derived = derived_properties(&Touch {
            medium: Some("email".into()),
            referrer_domain: Some("mail.example.com".into()),
            ..touch()
        });
        let held: BTreeMap<String, PropertyValue> = derived.into_iter().collect();
        assert_eq!(held[CHANNEL], PropertyValue::Text("email".to_string()));
        assert_eq!(
            held[CLASSIFIER],
            PropertyValue::Unsigned(CLASSIFIER_VERSION)
        );
    }
}
