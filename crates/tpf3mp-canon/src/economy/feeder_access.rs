//! Company-owned local feeder access at intercity stations
//! (`economy_feeder_access.lua`, model version 8 and later).
//!
//! A company's local road or tram line that serves an intercity station
//! makes that station easier to reach, which lowers the generalized cost of
//! the company's own corridor services there. Access is a derived fact,
//! rebuilt at every settlement from the current services, never stored.

use std::collections::{BTreeMap, BTreeSet};

use super::MarketKind;

/// Most access a feeder can give one station endpoint, in cents.
pub const CENTS_PER_ENDPOINT: i64 = 150;
/// Numerator of a feeder's frequency score: 150 cents at a ten-minute
/// headway.
pub const FREQUENCY_CENT_SECONDS: i64 = 90_000;

/// Market facts feeder access reads. Everything but the kind is free-form
/// metadata that may be absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeederMarket {
    pub kind: MarketKind,
    /// `"local"` for a town's own market, `"corridor"` between two towns.
    pub market_scope: Option<String>,
    pub town_a: Option<String>,
    pub town_b: Option<String>,
}

/// Service facts feeder access reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeederService {
    pub market_cid: String,
    pub company_cid: String,
    pub enabled: bool,
    pub capacity: i64,
    pub headway_seconds: i64,
    /// Native carrier, such as `"ROAD"`, `"TRAM"` or `"RAIL"`.
    pub carrier: Option<String>,
    /// Overrides the market's scope when present.
    pub market_scope: Option<String>,
    /// Overrides the market's towns when present.
    pub endpoint_town_cids: Option<Vec<String>>,
    /// Canonical station groups in stop order.
    pub station_group_cids: Option<Vec<String>>,
}

/// Best feeder access per company, town and station group, in cents.
pub type FeederIndex = BTreeMap<String, BTreeMap<String, BTreeMap<String, i64>>>;

fn scope<'a>(market: &'a FeederMarket, service: &'a FeederService) -> Option<&'a str> {
    service
        .market_scope
        .as_deref()
        .or(market.market_scope.as_deref())
}

/// The service's first and second endpoint towns: its own list when it has
/// one (even an empty one), otherwise the market's towns.
fn endpoint_towns<'a>(
    market: &'a FeederMarket,
    service: &'a FeederService,
) -> (Option<&'a str>, Option<&'a str>) {
    match &service.endpoint_town_cids {
        Some(towns) => (
            towns.first().map(String::as_str),
            towns.get(1).map(String::as_str),
        ),
        None => (market.town_a.as_deref(), market.town_b.as_deref()),
    }
}

/// `buildIndex`: the best access each enabled local road or tram line with
/// capacity gives at each of its distinct station groups, credited to its
/// company and first endpoint town.
///
/// A feeder's access is the weakest of 150 cents, its hourly capacity and
/// its frequency score (90000 / headway). Several feeders at one station
/// never stack: the best one counts. A line must serve at least two distinct
/// station groups, and a service whose market is unknown is skipped.
pub fn build_index(
    markets: &BTreeMap<String, FeederMarket>,
    services: &BTreeMap<String, FeederService>,
) -> FeederIndex {
    let mut index = FeederIndex::new();
    for service in services.values() {
        let Some(market) = markets.get(&service.market_cid) else {
            continue;
        };
        if market.kind == MarketKind::Cargo
            || scope(market, service) != Some("local")
            || !service.enabled
            || service.capacity <= 0
            || !matches!(service.carrier.as_deref(), Some("ROAD" | "TRAM"))
        {
            continue;
        }
        let groups: BTreeSet<&str> = service
            .station_group_cids
            .iter()
            .flatten()
            .map(String::as_str)
            .collect();
        // Both operands are positive, so Rust's division floors. A headway
        // too long for Lua to hold exactly gives zero in both languages.
        let frequency_cents = FREQUENCY_CENT_SECONDS / service.headway_seconds.max(1);
        let access_cents = CENTS_PER_ENDPOINT
            .min(service.capacity)
            .min(frequency_cents);
        let (Some(town), true) = (endpoint_towns(market, service).0, groups.len() >= 2) else {
            continue;
        };
        if access_cents <= 0 {
            continue;
        }
        let stations = index
            .entry(service.company_cid.clone())
            .or_default()
            .entry(town.to_owned())
            .or_default();
        for group in groups {
            let best = stations.entry(group.to_owned()).or_insert(access_cents);
            *best = (*best).max(access_cents);
        }
    }
    index
}

/// `cents`: feeder access of a corridor service, and how many of its two
/// endpoints (first and last station group) have access. Cargo markets,
/// services outside a corridor scope and services with fewer than two
/// station groups get none.
pub fn cents(market: &FeederMarket, service: &FeederService, index: &FeederIndex) -> (i64, i64) {
    if market.kind == MarketKind::Cargo || scope(market, service) != Some("corridor") {
        return (0, 0);
    }
    let groups = service.station_group_cids.as_deref().unwrap_or_default();
    let (Some(first_group), Some(last_group), true) =
        (groups.first(), groups.last(), groups.len() >= 2)
    else {
        return (0, 0);
    };
    let (first_town, second_town) = endpoint_towns(market, service);
    let company = index.get(&service.company_cid);
    let (mut total, mut count) = (0, 0);
    let mut seen = BTreeSet::new();
    for (town, group) in [(first_town, first_group), (second_town, last_group)] {
        let access_cents = town
            .and_then(|town| company?.get(town)?.get(group))
            .copied()
            .unwrap_or(0);
        // Lua deduplicates on `tostring(town) .. "\0" .. group`; the same key
        // keeps even colliding ids (with embedded NULs) identical.
        let key = format!("{}\0{group}", town.unwrap_or("nil"));
        if access_cents > 0 && seen.insert(key) {
            count += 1;
            total += access_cents;
        }
    }
    (total, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market(scope: &str, town_a: &str, town_b: &str) -> FeederMarket {
        FeederMarket {
            kind: MarketKind::Passenger,
            market_scope: Some(scope.into()),
            town_a: Some(town_a.into()),
            town_b: Some(town_b.into()),
        }
    }

    fn service(market_cid: &str, carrier: &str, headway: i64, groups: &[&str]) -> FeederService {
        FeederService {
            market_cid: market_cid.into(),
            company_cid: "company:1".into(),
            enabled: true,
            capacity: 300,
            headway_seconds: headway,
            carrier: Some(carrier.into()),
            market_scope: None,
            endpoint_town_cids: None,
            station_group_cids: Some(groups.iter().map(|group| (*group).to_owned()).collect()),
        }
    }

    #[test]
    fn a_local_bus_gives_its_company_access_at_the_intercity_station() {
        let markets = BTreeMap::from([
            (
                "market:corridor".to_owned(),
                market("corridor", "town:a", "town:b"),
            ),
            (
                "market:local".to_owned(),
                market("local", "town:a", "town:a"),
            ),
        ]);
        let bus = service(
            "market:local",
            "ROAD",
            900,
            &["station:suburb", "station:a"],
        );
        let services = BTreeMap::from([("line:bus".to_owned(), bus)]);
        let index = build_index(&markets, &services);
        // 90000 / 900 = 100 cents beats capacity and the 150 cent cap.
        assert_eq!(index["company:1"]["town:a"]["station:a"], 100);
        let rail = service("market:corridor", "RAIL", 600, &["station:a", "station:b"]);
        assert_eq!(cents(&markets["market:corridor"], &rail, &index), (100, 1));
        let rival = FeederService {
            company_cid: "company:2".into(),
            ..rail
        };
        assert_eq!(cents(&markets["market:corridor"], &rival, &index), (0, 0));
    }

    #[test]
    fn rail_and_single_station_lines_are_not_feeders() {
        let markets = BTreeMap::from([(
            "market:local".to_owned(),
            market("local", "town:a", "town:a"),
        )]);
        let services = BTreeMap::from([
            (
                "line:rail".to_owned(),
                service("market:local", "RAIL", 300, &["s:1", "s:2"]),
            ),
            (
                "line:loop".to_owned(),
                service("market:local", "TRAM", 300, &["s:1", "s:1"]),
            ),
        ]);
        assert!(build_index(&markets, &services).is_empty());
    }
}
