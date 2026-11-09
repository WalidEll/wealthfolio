//! CDG Capital Bourse market data provider for the Casablanca Stock Exchange.
//!
//! Covers equities listed on the Bourse de Casablanca (MIC `XCAS`, prices in
//! MAD) through the public, keyless API behind <https://www.cdgcapitalbourse.ma>.
//! The API is undocumented and unofficial (see `docs/morocco/cdg-api.md`), so
//! requests are kept few and slow and identify the application.
//!
//! Every call is a `POST /api/` with an RPC envelope
//! (`{"ACTIONS": [{"ACTION": {..}, "PARAMS": [..]}]}`); the answer is
//! `[{"<ACTION>": {"Valid": bool, "Data": ..}}]` with HTTP 200 even for bad
//! input. `Valid: false` (with `Data` a message string) is an upstream error;
//! `Valid: true` with an empty `Data` means the symbol is unknown. Numbers are
//! JSON numbers, or `""` when unavailable.
//!
//! - Latest quote: `VALEUR-SYNTHESE`. Volume is `NbrTitre` (shares); the API's
//!   `Volume` field is traded value in MAD and is never used as volume.
//! - History: `VALEUR-SESSIONS` (full OHLC and share quantity `Qte`, newest
//!   first, about the last 15 months, finalised sessions only). Older dates are
//!   filled from `VALEUR-GRAPH-APPL` (about 3 years), close only: its
//!   open/high/low are unreliable and its volume is a MAD value. Today's price
//!   comes from the latest quote, not from history.
//! - Search: `QUICK-SEARCH`, stocks only (`type == "A"`); funds and indices are
//!   dropped.
//! - Profile: `VALEUR-SYNTHESE` (name, ISIN, market cap) plus `VALEURS-INFOS`
//!   (sector, website, description) as optional enrichment, in two separate
//!   requests. Country of domicile is not inferred from the venue.
//!
//! Timestamps are Casablanca local time without an offset. Morocco is UTC+1
//! except during Ramadan (UTC+0); this crate has no time zone database, so a
//! fixed +01:00 is used. Daily bars are stamped at the session close, which
//! keeps their date right either way; intraday latest-quote timestamps are one
//! hour early during Ramadan (never in the future).
//!
//! Splits and dividends are unsupported: the API has no split feed, and the
//! only dividend data (`AGENDA-ALL`, latest ex-date in `VALEURS-INFOS`) carries
//! no amounts in the captured responses. Dividends are a phase 2 item.

use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use log::debug;
use reqwest::Client;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::time::Duration;

use crate::errors::MarketDataError;
use crate::models::{
    AssetProfile, Coverage, InstrumentKind, ProviderId, ProviderInstrument, Quote, QuoteContext,
    SearchResult,
};
use crate::provider::{MarketDataProvider, ProviderCapabilities, RateLimit};
use crate::registry::{RateLimitConfig, RateLimiter};

const PROVIDER_ID: &str = "CDG_CAPITAL";
const MIC: &str = "XCAS";
const EXCHANGE_NAME: &str = "Casablanca";
const CURRENCY: &str = "MAD";
const API_URL: &str = "https://www.cdgcapitalbourse.ma/api/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!(
    "Wealthfolio/",
    env!("CARGO_PKG_VERSION"),
    " (+https://wealthfolio.app)"
);
/// Casablanca outside Ramadan. See the module docs for the Ramadan caveat.
const CASABLANCA_UTC_OFFSET_SECS: i32 = 3600;
/// End of the trading day (closing auction), used as the timestamp of daily
/// bars. Unverified against an after-close capture; the date is right either way.
const SESSION_CLOSE: (u32, u32) = (15, 30);
/// The server returns at most about 306 sessions whatever `Top_` asks for.
const MAX_SESSIONS_TOP: i64 = 1000;
/// Extra rows asked of `VALEUR-SESSIONS` beyond the calendar-day bound.
const SESSIONS_TOP_SLACK: i64 = 5;
const DATE_TIME_FORMAT: &str = "%d/%m/%Y %H:%M:%S";

// ---------------------------------------------------------------------------
// Wire model
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(rename = "Valid")]
    valid: bool,
    #[serde(rename = "Data", default)]
    data: Option<Box<RawValue>>,
}

/// Parse a JSON number or numeric string from its literal, so 692.6989 stays
/// exact. `""`, `null` and anything unparsable read as missing.
fn decimal_from_raw(raw: &str) -> Option<Decimal> {
    let raw = raw.trim();
    if raw.starts_with('"') {
        let text: String = serde_json::from_str(raw).ok()?;
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        return Decimal::from_str(text).ok();
    }
    if raw == "null" {
        return None;
    }
    Decimal::from_str(raw)
        .or_else(|_| Decimal::from_scientific(raw))
        .ok()
}

fn de_decimal<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Decimal>, D::Error> {
    let raw = Box::<RawValue>::deserialize(deserializer)?;
    Ok(decimal_from_raw(raw.get()))
}

/// Text field that may also arrive as a number (`Id`) or `""`.
fn de_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    })
}

/// `VALEUR-SYNTHESE` row.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SyntheseRow {
    #[serde(default, deserialize_with = "de_text")]
    symbol: Option<String>,
    #[serde(default, deserialize_with = "de_text")]
    libelle: Option<String>,
    #[serde(default, deserialize_with = "de_decimal")]
    cours: Option<Decimal>,
    #[serde(default, deserialize_with = "de_text")]
    date_cours: Option<String>,
    /// ISIN; the field name is misspelled upstream.
    #[serde(default, deserialize_with = "de_text")]
    ising: Option<String>,
    #[serde(default, deserialize_with = "de_decimal")]
    ouverture: Option<Decimal>,
    #[serde(default, deserialize_with = "de_decimal")]
    plus_haut: Option<Decimal>,
    #[serde(default, deserialize_with = "de_decimal")]
    plus_bas: Option<Decimal>,
    /// Shares traded today.
    #[serde(default, deserialize_with = "de_decimal")]
    nbr_titre: Option<Decimal>,
    #[serde(default, deserialize_with = "de_decimal")]
    capitalisation: Option<Decimal>,
}

/// `VALEUR-SESSIONS` row (one finalised trading day).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SessionRow {
    #[serde(default, deserialize_with = "de_decimal")]
    cours: Option<Decimal>,
    #[serde(default, deserialize_with = "de_text")]
    date_cours: Option<String>,
    #[serde(default, deserialize_with = "de_decimal")]
    ouverture: Option<Decimal>,
    #[serde(default, deserialize_with = "de_decimal")]
    plus_haut: Option<Decimal>,
    #[serde(default, deserialize_with = "de_decimal")]
    plus_bas: Option<Decimal>,
    /// Shares traded.
    #[serde(default, deserialize_with = "de_decimal")]
    qte: Option<Decimal>,
}

/// `VALEUR-GRAPH-APPL` row. Only the date and close are reliable.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GraphRow {
    #[serde(default, deserialize_with = "de_text")]
    date_cotation: Option<String>,
    #[serde(default, deserialize_with = "de_decimal")]
    cloture: Option<Decimal>,
}

/// `QUICK-SEARCH` row.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SearchRow {
    #[serde(default, deserialize_with = "de_text")]
    symbol: Option<String>,
    #[serde(default, deserialize_with = "de_text")]
    libelle: Option<String>,
    /// `A` stock, `O` fund, `I` index.
    #[serde(rename = "type", default, deserialize_with = "de_text")]
    kind: Option<String>,
}

/// `VALEURS-INFOS` row.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InfosRow {
    #[serde(default, deserialize_with = "de_text")]
    presentation: Option<String>,
    #[serde(default, deserialize_with = "de_text")]
    site_internet: Option<String>,
    #[serde(default, deserialize_with = "de_text")]
    secteur: Option<String>,
}

// ---------------------------------------------------------------------------
// Envelope (unit tested)
// ---------------------------------------------------------------------------

/// Parameter types of the request envelope: `S` string, `I` integer (still
/// sent as a string).
#[derive(Clone, Copy, Debug)]
enum ParamType {
    S,
    I,
}

impl ParamType {
    fn code(self) -> &'static str {
        match self {
            Self::S => "S",
            Self::I => "I",
        }
    }
}

/// Build the request body for one action. `Lang_` and `Espace_` are sent on
/// every action, as the site does.
fn request_body(action: &str, params: &[(&str, ParamType, &str)]) -> Value {
    let mut all = vec![
        json!({ "NAME": "Lang_", "TYPE": "S", "VALUE": "fr" }),
        json!({ "NAME": "Espace_", "TYPE": "I", "VALUE": "1" }),
    ];
    all.extend(
        params.iter().map(
            |(name, kind, value)| json!({ "NAME": name, "TYPE": kind.code(), "VALUE": value }),
        ),
    );
    json!({
        "ACTIONS": [{
            "ACTION": { "NAME": action, "TYPE": "SELECT", "VALUE": action },
            "PARAMS": all,
        }]
    })
}

fn provider_error(message: String) -> MarketDataError {
    MarketDataError::ProviderError {
        provider: PROVIDER_ID.to_string(),
        message,
    }
}

/// Unwrap `[0][action]` of a response body into its `Data` rows.
///
/// `Valid: false` is a provider error. `Valid: true` with no rows is returned
/// as an empty list; callers decide whether that means "not found".
fn decode_rows<T: DeserializeOwned>(action: &str, body: &[u8]) -> Result<Vec<T>, MarketDataError> {
    let mut response: Vec<HashMap<String, Envelope>> = serde_json::from_slice(body)
        .map_err(|error| provider_error(format!("{action}: JSON parse error: {error}")))?;
    let envelope = response
        .first_mut()
        .and_then(|actions| actions.remove(action))
        .ok_or_else(|| provider_error(format!("{action}: missing from response")))?;
    if !envelope.valid {
        return Err(provider_error(format!(
            "{action}: request rejected (Valid=false)"
        )));
    }
    match envelope.data {
        None => Ok(Vec::new()),
        Some(data) if data.get().trim() == "null" => Ok(Vec::new()),
        Some(data) => serde_json::from_str(data.get())
            .map_err(|error| provider_error(format!("{action}: unexpected Data: {error}"))),
    }
}

// ---------------------------------------------------------------------------
// Pure mapping helpers (unit tested)
// ---------------------------------------------------------------------------

fn casablanca() -> FixedOffset {
    FixedOffset::east_opt(CASABLANCA_UTC_OFFSET_SECS).expect("valid Casablanca offset")
}

fn parse_local(text: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(text.trim(), DATE_TIME_FORMAT).ok()
}

fn parse_date(text: &str) -> Option<NaiveDate> {
    parse_local(text).map(|local| local.date())
}

fn casablanca_datetime_utc(text: &str) -> Option<DateTime<Utc>> {
    casablanca()
        .from_local_datetime(&parse_local(text)?)
        .single()
        .map(|dt| dt.with_timezone(&Utc))
}

fn session_close_utc(date: NaiveDate) -> DateTime<Utc> {
    let close = NaiveTime::from_hms_opt(SESSION_CLOSE.0, SESSION_CLOSE.1, 0).expect("valid time");
    casablanca()
        .from_local_datetime(&date.and_time(close))
        .single()
        .expect("fixed offsets are unambiguous")
        .with_timezone(&Utc)
}

/// Calendar dates of a history request. Daily bars are stamped 14:30 UTC on
/// their trading date, so the UTC dates of the range are the trading dates it
/// covers.
fn history_dates(start: DateTime<Utc>, end: DateTime<Utc>) -> (NaiveDate, NaiveDate) {
    (start.date_naive(), end.date_naive())
}

/// `Top_` for `VALEUR-SESSIONS`: rows are newest first, one per trading day,
/// so the calendar days since `from` bound the sessions needed.
fn sessions_top(from: NaiveDate, today: NaiveDate) -> i64 {
    ((today - from).num_days() + 1 + SESSIONS_TOP_SLACK).clamp(1, MAX_SESSIONS_TOP)
}

fn positive(value: Option<Decimal>) -> Option<Decimal> {
    value.filter(|value| *value > Decimal::ZERO)
}

fn not_found(symbol: &str) -> MarketDataError {
    MarketDataError::SymbolNotFound(format!("{symbol}@{MIC}"))
}

fn latest_quote_from_rows(rows: &[SyntheseRow], symbol: &str) -> Result<Quote, MarketDataError> {
    let row = rows.first().ok_or_else(|| not_found(symbol))?;
    // A listed symbol without a usable last trade is "no data", not "unknown".
    let close = positive(row.cours).ok_or(MarketDataError::NoDataForRange)?;
    let timestamp = row
        .date_cours
        .as_deref()
        .and_then(casablanca_datetime_utc)
        .ok_or(MarketDataError::NoDataForRange)?;
    Ok(Quote {
        timestamp,
        open: positive(row.ouverture),
        high: positive(row.plus_haut),
        low: positive(row.plus_bas),
        close,
        volume: row.nbr_titre,
        currency: CURRENCY.to_string(),
        source: PROVIDER_ID.to_string(),
    })
}

/// Earliest date `VALEUR-SESSIONS` returned; `None` when it returned no usable
/// row (an unknown symbol).
fn earliest_session(sessions: &[SessionRow]) -> Option<NaiveDate> {
    sessions
        .iter()
        .filter_map(|row| row.date_cours.as_deref().and_then(parse_date))
        .min()
}

/// Whether the range starts before the sessions `VALEUR-SESSIONS` keeps, so
/// `VALEUR-GRAPH-APPL` must extend it.
fn needs_graph_extension(sessions: &[SessionRow], from: NaiveDate) -> bool {
    earliest_session(sessions).is_some_and(|earliest| from < earliest)
}

/// Daily bars in `[from, till]`, oldest first. Sessions win; graph rows only
/// fill dates before the earliest session and carry the close alone.
fn history_from_rows(
    sessions: &[SessionRow],
    graph: &[GraphRow],
    from: NaiveDate,
    till: NaiveDate,
) -> Vec<Quote> {
    let in_range = |date: &NaiveDate| *date >= from && *date <= till;
    let mut bars: BTreeMap<NaiveDate, Quote> = BTreeMap::new();

    if let Some(earliest) = earliest_session(sessions) {
        for row in graph {
            let Some(date) = row.date_cotation.as_deref().and_then(parse_date) else {
                continue;
            };
            let Some(close) = positive(row.cloture) else {
                continue;
            };
            if date >= earliest || !in_range(&date) {
                continue;
            }
            bars.insert(date, daily_quote(date, close, None, None, None, None));
        }
    }

    for row in sessions {
        let Some(date) = row.date_cours.as_deref().and_then(parse_date) else {
            continue;
        };
        let Some(close) = positive(row.cours) else {
            continue;
        };
        if !in_range(&date) {
            continue;
        }
        bars.insert(
            date,
            daily_quote(
                date,
                close,
                positive(row.ouverture),
                positive(row.plus_haut),
                positive(row.plus_bas),
                row.qte,
            ),
        );
    }

    bars.into_values().collect()
}

fn daily_quote(
    date: NaiveDate,
    close: Decimal,
    open: Option<Decimal>,
    high: Option<Decimal>,
    low: Option<Decimal>,
    volume: Option<Decimal>,
) -> Quote {
    Quote {
        timestamp: session_close_utc(date),
        open,
        high,
        low,
        close,
        volume,
        currency: CURRENCY.to_string(),
        source: PROVIDER_ID.to_string(),
    }
}

fn search_results_from_rows(rows: Vec<SearchRow>) -> Vec<SearchResult> {
    rows.into_iter()
        .filter(|row| {
            row.kind
                .as_deref()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("A"))
        })
        .filter_map(|row| {
            let symbol = row.symbol?.to_uppercase();
            let name = row.libelle.unwrap_or_else(|| symbol.clone());
            Some(
                SearchResult::new(symbol, name, MIC, "EQUITY")
                    .with_exchange_mic(MIC)
                    .with_exchange_name(EXCHANGE_NAME)
                    .with_currency(CURRENCY)
                    .with_data_source(PROVIDER_ID),
            )
        })
        .collect()
}

/// Identity from `VALEUR-SYNTHESE`; required.
fn profile_from_synthese(
    rows: &[SyntheseRow],
    symbol: &str,
) -> Result<AssetProfile, MarketDataError> {
    let row = rows.first().ok_or_else(|| not_found(symbol))?;
    let mut profile = AssetProfile {
        source: Some(PROVIDER_ID.to_string()),
        name: row.libelle.clone().or_else(|| row.symbol.clone()),
        quote_type: Some("EQUITY".to_string()),
        currency: Some(CURRENCY.to_string()),
        exchange: Some(MIC.to_string()),
        market_cap: positive(row.capitalisation).and_then(|value| value.to_f64()),
        ..Default::default()
    };
    if let Some(isin) = &row.ising {
        profile = profile.isin(isin.to_uppercase());
    }
    Ok(profile)
}

/// Company details from `VALEURS-INFOS`; optional enrichment.
fn enrich_profile(profile: &mut AssetProfile, rows: &[InfosRow]) {
    let Some(row) = rows.first() else {
        return;
    };
    profile.sector = row.secteur.clone();
    profile.description = row.presentation.clone();
    profile.website = row.site_internet.as_deref().map(|site| {
        if site.starts_with("http://") || site.starts_with("https://") {
            site.to_string()
        } else {
            format!("https://{site}")
        }
    });
}

/// Bourse tickers are short upper-case alphanumerics. Rejecting anything else
/// keeps arbitrary input out of upstream requests.
fn normalize_symbol(symbol: &str) -> Result<String, MarketDataError> {
    let ticker = symbol.trim().to_uppercase();
    let valid = !ticker.is_empty()
        && ticker.len() <= 20
        && ticker
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(ticker)
    } else {
        Err(MarketDataError::SymbolNotFound(symbol.to_string()))
    }
}

fn symbol_from_instrument(instrument: &ProviderInstrument) -> Result<String, MarketDataError> {
    match instrument {
        ProviderInstrument::EquitySymbol { symbol } => normalize_symbol(symbol),
        other => Err(MarketDataError::UnsupportedAssetType(format!(
            "{:?}",
            other
        ))),
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct CdgCapitalProvider {
    client: Client,
    request_limiter: RateLimiter,
}

impl Default for CdgCapitalProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CdgCapitalProvider {
    pub fn new() -> Self {
        let client = wealthfolio_http::client_builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .unwrap_or_else(|_| wealthfolio_http::client());
        let request_limiter = RateLimiter::new();
        let provider_id: ProviderId = Cow::Borrowed(PROVIDER_ID);
        // History and profile can issue two requests each; meter every one.
        request_limiter.configure(
            &provider_id,
            RateLimitConfig {
                requests_per_minute: 30,
                burst_capacity: 2.0,
            },
        );
        Self {
            client,
            request_limiter,
        }
    }

    /// Send one action and return its `Data` rows.
    async fn call<T: DeserializeOwned>(
        &self,
        action: &str,
        params: &[(&str, ParamType, &str)],
    ) -> Result<Vec<T>, MarketDataError> {
        let provider_id: ProviderId = Cow::Borrowed(PROVIDER_ID);
        self.request_limiter.acquire(&provider_id).await;
        debug!("{} POST {}", PROVIDER_ID, action);
        let response = self
            .client
            .post(API_URL)
            .json(&request_body(action, params))
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    MarketDataError::Timeout {
                        provider: PROVIDER_ID.to_string(),
                    }
                } else {
                    provider_error(format!("HTTP request failed: {error}"))
                }
            })?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(MarketDataError::RateLimited {
                provider: PROVIDER_ID.to_string(),
            });
        }
        if !response.status().is_success() {
            return Err(provider_error(format!("HTTP {}", response.status())));
        }
        let body = response.bytes().await.map_err(|error| {
            if error.is_timeout() {
                MarketDataError::Timeout {
                    provider: PROVIDER_ID.to_string(),
                }
            } else {
                provider_error(format!("HTTP body read failed: {error}"))
            }
        })?;
        decode_rows(action, &body)
    }

    async fn synthese(&self, symbol: &str) -> Result<Vec<SyntheseRow>, MarketDataError> {
        self.call(
            "VALEUR-SYNTHESE",
            &[
                ("Symbol_", ParamType::S, symbol),
                ("IdPartener_", ParamType::I, "1"),
            ],
        )
        .await
    }
}

#[async_trait]
impl MarketDataProvider for CdgCapitalProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> u8 {
        14
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            instrument_kinds: &[InstrumentKind::Equity],
            coverage: Coverage::casablanca(),
            supports_latest: true,
            supports_historical: true,
            supports_search: true,
            supports_profile: true,
            supports_dividends: false,
        }
    }

    fn rate_limit(&self) -> RateLimit {
        RateLimit {
            requests_per_minute: 30,
            max_concurrency: 1,
            min_delay: Duration::from_millis(500),
        }
    }

    async fn get_latest_quote(
        &self,
        _context: &QuoteContext,
        instrument: ProviderInstrument,
    ) -> Result<Quote, MarketDataError> {
        let symbol = symbol_from_instrument(&instrument)?;
        let rows = self.synthese(&symbol).await?;
        latest_quote_from_rows(&rows, &symbol)
    }

    async fn get_historical_quotes(
        &self,
        _context: &QuoteContext,
        instrument: ProviderInstrument,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Quote>, MarketDataError> {
        let symbol = symbol_from_instrument(&instrument)?;
        let (from, till) = history_dates(start, end);
        if from > till {
            return Ok(Vec::new());
        }

        let top = sessions_top(from, Utc::now().date_naive()).to_string();
        let sessions: Vec<SessionRow> = self
            .call(
                "VALEUR-SESSIONS",
                &[
                    ("Symbol_", ParamType::S, &symbol),
                    ("Top_", ParamType::I, &top),
                ],
            )
            .await?;
        if earliest_session(&sessions).is_none() {
            return Err(not_found(&symbol));
        }

        let graph: Vec<GraphRow> = if needs_graph_extension(&sessions, from) {
            self.call("VALEUR-GRAPH-APPL", &[("Symbol_", ParamType::S, &symbol)])
                .await?
        } else {
            Vec::new()
        };

        Ok(history_from_rows(&sessions, &graph, from, till))
    }

    async fn search(&self, query: &str) -> Result<Vec<SearchResult>, MarketDataError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<SearchRow> = self
            .call(
                "QUICK-SEARCH",
                &[
                    ("IdPartener_", ParamType::I, "1"),
                    ("Query_", ParamType::S, query),
                ],
            )
            .await?;
        Ok(search_results_from_rows(rows))
    }

    async fn get_profile(&self, symbol: &str) -> Result<AssetProfile, MarketDataError> {
        // The registry may pass "XCAS:ATW" or a bare ticker.
        let bare = symbol
            .split_once(':')
            .map(|(_, ticker)| ticker)
            .unwrap_or(symbol);
        let ticker = normalize_symbol(bare)?;
        let synthese = self.synthese(&ticker).await?;
        let mut profile = profile_from_synthese(&synthese, &ticker)?;
        // Optional enrichment must not discard the security's identity on failure.
        match self
            .call::<InfosRow>("VALEURS-INFOS", &[("Symbol_", ParamType::S, &ticker)])
            .await
        {
            Ok(infos) => enrich_profile(&mut profile, &infos),
            Err(error) => debug!("{} VALEURS-INFOS skipped: {}", PROVIDER_ID, error),
        }
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::InstrumentId;
    use std::sync::Arc;

    const SYNTHESE_ATW: &str = include_str!("fixtures/VALEUR-SYNTHESE_ATW.json");
    const SYNTHESE_CIH: &str = include_str!("fixtures/VALEUR-SYNTHESE_CIH.json");
    const SYNTHESE_IAM: &str = include_str!("fixtures/VALEUR-SYNTHESE_IAM.json");
    const SYNTHESE_ZZZZ: &str = include_str!("fixtures/VALEUR-SYNTHESE_ZZZZ.json");
    const SESSIONS_ATW: &str = include_str!("fixtures/VALEUR-SESSIONS_ATW.json");
    const SESSIONS_IAM: &str = include_str!("fixtures/VALEUR-SESSIONS_IAM.json");
    const SESSIONS_CIH: &str = include_str!("fixtures/VALEUR-SESSIONS_CIH.json");
    const SESSIONS_ZZZZ: &str = include_str!("fixtures/VALEUR-SESSIONS_ZZZZ.json");
    const GRAPH_ATW: &str = include_str!("fixtures/VALEUR-GRAPH-APPL_ATW.json");
    const GRAPH_ZZZZ: &str = include_str!("fixtures/VALEUR-GRAPH-APPL_ZZZZ.json");
    const SEARCH_ATTIJARI: &str = include_str!("fixtures/QUICK-SEARCH_attijari.json");
    const SEARCH_BANK: &str = include_str!("fixtures/QUICK-SEARCH_bank.json");
    const SEARCH_CIH: &str = include_str!("fixtures/QUICK-SEARCH_cih.json");
    const SEARCH_ZZZZ: &str = include_str!("fixtures/QUICK-SEARCH_ZZZZ.json");
    const INFOS_ATW: &str = include_str!("fixtures/VALEURS-INFOS_ATW.json");
    const INFOS_IAM: &str = include_str!("fixtures/VALEURS-INFOS_IAM.json");
    const INFOS_ZZZZ: &str = include_str!("fixtures/VALEURS-INFOS_ZZZZ.json");
    const PALMARES_ERROR: &str = include_str!("fixtures/PALMARES-STOCKS_error.json");

    fn rows<T: DeserializeOwned>(action: &str, fixture: &str) -> Vec<T> {
        decode_rows(action, fixture.as_bytes()).expect("fixture decodes")
    }

    fn dec(text: &str) -> Decimal {
        Decimal::from_str(text).unwrap()
    }

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    fn equity(mic: Option<&'static str>) -> InstrumentId {
        InstrumentId::Equity {
            ticker: Arc::from("ATW"),
            mic: mic.map(Cow::Borrowed),
        }
    }

    #[test]
    fn provider_identity_and_capabilities() {
        // Construction builds a client only; no request is sent.
        let provider = CdgCapitalProvider::new();
        assert_eq!(provider.id(), "CDG_CAPITAL");
        let caps = provider.capabilities();
        assert!(caps.supports_latest && caps.supports_historical);
        assert!(caps.supports_search && caps.supports_profile);
        assert!(!caps.supports_dividends);
        assert!(caps.supports_instrument(&equity(Some("XCAS"))));
        assert!(!caps.supports_instrument(&equity(Some("XPAR"))));
        assert!(!caps.supports_instrument(&equity(None)));
        assert!(provider.rate_limit().requests_per_minute <= 30);
    }

    #[test]
    fn request_body_matches_the_site_envelope() {
        let body = request_body(
            "VALEUR-SESSIONS",
            &[
                ("Symbol_", ParamType::S, "ATW"),
                ("Top_", ParamType::I, "5"),
            ],
        );
        assert_eq!(
            body,
            json!({"ACTIONS": [{
                "ACTION": {"NAME": "VALEUR-SESSIONS", "TYPE": "SELECT", "VALUE": "VALEUR-SESSIONS"},
                "PARAMS": [
                    {"NAME": "Lang_", "TYPE": "S", "VALUE": "fr"},
                    {"NAME": "Espace_", "TYPE": "I", "VALUE": "1"},
                    {"NAME": "Symbol_", "TYPE": "S", "VALUE": "ATW"},
                    {"NAME": "Top_", "TYPE": "I", "VALUE": "5"}
                ]
            }]})
        );
    }

    #[test]
    fn latest_quote_maps_synthese() {
        let quote = latest_quote_from_rows(&rows("VALEUR-SYNTHESE", SYNTHESE_ATW), "ATW").unwrap();
        assert_eq!(quote.close, dec("690.2"));
        assert_eq!(quote.open, Some(dec("693.9")));
        assert_eq!(quote.high, Some(dec("694")));
        assert_eq!(quote.low, Some(dec("682")));
        // NbrTitre (shares), not Volume (MAD value 14439308.9).
        assert_eq!(quote.volume, Some(Decimal::from(20_845)));
        assert_eq!(quote.currency, "MAD");
        assert_eq!(quote.source, "CDG_CAPITAL");
        // 09/11/2026 14:36:47 Casablanca (+01:00) = 13:36:47 UTC.
        assert_eq!(
            quote.timestamp,
            Utc.with_ymd_and_hms(2026, 11, 9, 13, 36, 47).unwrap()
        );

        let cih = latest_quote_from_rows(&rows("VALEUR-SYNTHESE", SYNTHESE_CIH), "CIH").unwrap();
        assert_eq!(cih.close, dec("310"));
        let iam = latest_quote_from_rows(&rows("VALEUR-SYNTHESE", SYNTHESE_IAM), "IAM").unwrap();
        assert_eq!(iam.close, dec("89.51"));
        assert_eq!(iam.volume, Some(Decimal::from(35_635)));
    }

    #[test]
    fn unknown_symbol_is_not_found() {
        let empty: Vec<SyntheseRow> = rows("VALEUR-SYNTHESE", SYNTHESE_ZZZZ);
        assert!(empty.is_empty());
        assert!(matches!(
            latest_quote_from_rows(&empty, "ZZZZ"),
            Err(MarketDataError::SymbolNotFound(_))
        ));
        assert!(matches!(
            profile_from_synthese(&empty, "ZZZZ"),
            Err(MarketDataError::SymbolNotFound(_))
        ));
        let sessions: Vec<SessionRow> = rows("VALEUR-SESSIONS", SESSIONS_ZZZZ);
        assert!(earliest_session(&sessions).is_none());
        let search: Vec<SearchRow> = rows("QUICK-SEARCH", SEARCH_ZZZZ);
        assert!(search_results_from_rows(search).is_empty());
    }

    #[test]
    fn error_envelope_is_a_provider_error() {
        let result = decode_rows::<SyntheseRow>("PALMARES-STOCKS", PALMARES_ERROR.as_bytes());
        let error = result.unwrap_err();
        assert!(matches!(error, MarketDataError::ProviderError { .. }));
        // The registry moves on to the next provider; it is never terminal.
        assert_ne!(error.retry_class(), crate::errors::RetryClass::Never);
    }

    #[test]
    fn envelope_for_another_action_or_garbage_is_a_provider_error() {
        assert!(matches!(
            decode_rows::<SyntheseRow>("VALEUR-SYNTHESE", SESSIONS_ATW.as_bytes()),
            Err(MarketDataError::ProviderError { .. })
        ));
        assert!(matches!(
            decode_rows::<SyntheseRow>("VALEUR-SYNTHESE", b"<html>"),
            Err(MarketDataError::ProviderError { .. })
        ));
        assert!(matches!(
            decode_rows::<SyntheseRow>("VALEUR-SYNTHESE", b"[]"),
            Err(MarketDataError::ProviderError { .. })
        ));
    }

    #[test]
    fn empty_string_numerics_read_as_missing() {
        assert_eq!(decimal_from_raw(r#""""#), None);
        assert_eq!(decimal_from_raw(r#"" 12.5 ""#), Some(dec("12.5")));
        assert_eq!(decimal_from_raw("null"), None);
        assert_eq!(decimal_from_raw("692.6989"), Some(dec("692.6989")));
        assert_eq!(decimal_from_raw("1e3"), Some(Decimal::from(1000)));

        let row: SyntheseRow = serde_json::from_str(
            r#"{"Cours": "", "DateCours": "09/11/2026 10:00:00", "Ouverture": "", "NbrTitre": ""}"#,
        )
        .unwrap();
        assert_eq!(row.cours, None);
        assert_eq!(row.ouverture, None);
        assert!(matches!(
            latest_quote_from_rows(&[row], "ATW"),
            Err(MarketDataError::NoDataForRange)
        ));
    }

    #[test]
    fn decimals_keep_the_json_literal() {
        let row: SyntheseRow =
            serde_json::from_str(r#"{"Cours": 0.1000000000000000000000000001}"#).unwrap();
        assert_eq!(row.cours, Some(dec("0.1000000000000000000000000001")));
    }

    #[test]
    fn history_is_oldest_first_and_stamped_at_session_close() {
        let sessions: Vec<SessionRow> = rows("VALEUR-SESSIONS", SESSIONS_ATW);
        let quotes = history_from_rows(&sessions, &[], date("2026-10-30"), date("2026-11-05"));
        let dates: Vec<NaiveDate> = quotes.iter().map(|q| q.timestamp.date_naive()).collect();
        assert_eq!(
            dates,
            vec![
                date("2026-10-30"),
                date("2026-11-02"),
                date("2026-11-03"),
                date("2026-11-04"),
                date("2026-11-05"),
            ]
        );
        let last = quotes.last().unwrap();
        assert_eq!(last.close, dec("694.6"));
        assert_eq!(last.open, Some(dec("670")));
        assert_eq!(last.high, Some(dec("694.6")));
        assert_eq!(last.low, Some(dec("670")));
        assert_eq!(last.volume, Some(Decimal::from(34_194)));
        assert!(quotes
            .iter()
            .all(|q| q.currency == "MAD" && q.source == "CDG_CAPITAL"));
        // 15:30 Casablanca (+01:00) = 14:30 UTC on the trading date.
        assert_eq!(
            last.timestamp,
            Utc.with_ymd_and_hms(2026, 11, 5, 14, 30, 0).unwrap()
        );

        for fixture in [SESSIONS_IAM, SESSIONS_CIH] {
            let sessions: Vec<SessionRow> = rows("VALEUR-SESSIONS", fixture);
            let quotes = history_from_rows(&sessions, &[], date("2026-01-01"), date("2026-12-31"));
            assert!(!quotes.is_empty());
            assert!(quotes.windows(2).all(|w| w[0].timestamp < w[1].timestamp));
        }
    }

    #[test]
    fn history_filters_to_the_requested_range() {
        let sessions: Vec<SessionRow> = rows("VALEUR-SESSIONS", SESSIONS_ATW);
        let quotes = history_from_rows(&sessions, &[], date("2026-11-03"), date("2026-11-04"));
        assert_eq!(quotes.len(), 2);
        assert_eq!(quotes[0].close, dec("662.9"));
        assert_eq!(quotes[1].close, dec("670"));
    }

    #[test]
    fn history_empty_range_for_known_symbol() {
        // Today's session is not in VALEUR-SESSIONS until finalised: no bars,
        // and no graph extension because the range is after the sessions.
        let sessions: Vec<SessionRow> = rows("VALEUR-SESSIONS", SESSIONS_ATW);
        let today = date("2026-11-09");
        assert!(!needs_graph_extension(&sessions, today));
        assert!(history_from_rows(&sessions, &[], today, today).is_empty());
    }

    #[test]
    fn history_extends_with_graph_close_only_before_sessions() {
        let sessions: Vec<SessionRow> = rows("VALEUR-SESSIONS", SESSIONS_ATW);
        let graph: Vec<GraphRow> = rows("VALEUR-GRAPH-APPL", GRAPH_ATW);
        let from = date("2023-11-01");
        assert!(needs_graph_extension(&sessions, from));
        assert!(!needs_graph_extension(&sessions, date("2026-10-30")));

        let quotes = history_from_rows(&sessions, &graph, from, date("2026-11-05"));
        assert_eq!(quotes.len(), 4 + 5);
        let first = &quotes[0];
        assert_eq!(first.timestamp.date_naive(), date("2023-11-10"));
        assert_eq!(first.close, dec("458"));
        assert_eq!((first.open, first.high, first.low), (None, None, None));
        // GRAPH-APPL Volume is a MAD value, never used as quantity.
        assert_eq!(first.volume, None);
        assert!(quotes.windows(2).all(|w| w[0].timestamp < w[1].timestamp));

        // A graph row on a session date never overrides the session's bar.
        let overlapping: Vec<GraphRow> = serde_json::from_str(
            r#"[{"DateCotation": "05/11/2026 00:00:00", "Ouverture": 670, "PlusHaut": 670,
                 "PlusBas": 670, "Cloture": 1, "Volume": 1}]"#,
        )
        .unwrap();
        let quotes = history_from_rows(&sessions, &overlapping, from, date("2026-11-05"));
        assert_eq!(quotes.len(), 5);
        assert_eq!(quotes.last().unwrap().close, dec("694.6"));

        let graph_empty: Vec<GraphRow> = rows("VALEUR-GRAPH-APPL", GRAPH_ZZZZ);
        assert!(graph_empty.is_empty());
    }

    #[test]
    fn sessions_top_bounds_rows_needed() {
        let today = date("2026-11-09");
        assert_eq!(sessions_top(today, today), 1 + SESSIONS_TOP_SLACK);
        assert_eq!(
            sessions_top(date("2026-11-02"), today),
            8 + SESSIONS_TOP_SLACK
        );
        assert_eq!(sessions_top(date("2020-01-01"), today), MAX_SESSIONS_TOP);
        assert_eq!(sessions_top(date("2027-01-01"), today), 1);
    }

    #[test]
    fn history_dates_stay_on_the_utc_calendar() {
        let start = Utc.with_ymd_and_hms(2026, 11, 5, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 11, 5, 23, 59, 59).unwrap();
        let day = date("2026-11-05");
        assert_eq!(history_dates(start, end), (day, day));
        assert_eq!(session_close_utc(day).date_naive(), day);
    }

    #[test]
    fn search_keeps_stocks_only() {
        let results = search_results_from_rows(rows("QUICK-SEARCH", SEARCH_BANK));
        let symbols: Vec<&str> = results.iter().map(|r| r.symbol.as_str()).collect();
        // The BANK index (type I) is dropped.
        assert_eq!(symbols, vec!["ATW", "BOA", "CFG"]);
        let atw = &results[0];
        assert_eq!(atw.name, "ATTIJARIWAFA BANK");
        assert_eq!(atw.exchange_mic.as_deref(), Some("XCAS"));
        assert_eq!(atw.exchange_name.as_deref(), Some("Casablanca"));
        assert_eq!(atw.currency.as_deref(), Some("MAD"));
        assert_eq!(atw.asset_type, "EQUITY");
        assert_eq!(atw.data_source.as_deref(), Some("CDG_CAPITAL"));

        // FCP funds (type O, ISIN symbols) are dropped.
        let results = search_results_from_rows(rows("QUICK-SEARCH", SEARCH_CIH));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol, "CIH");
        assert_eq!(results[0].name, "Crédit Immobilier et Hôtelier");

        let results = search_results_from_rows(rows("QUICK-SEARCH", SEARCH_ATTIJARI));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol, "ATW");
    }

    #[test]
    fn profile_combines_synthese_and_infos() {
        let mut profile =
            profile_from_synthese(&rows("VALEUR-SYNTHESE", SYNTHESE_ATW), "ATW").unwrap();
        enrich_profile(&mut profile, &rows("VALEURS-INFOS", INFOS_ATW));
        assert_eq!(profile.source.as_deref(), Some("CDG_CAPITAL"));
        assert_eq!(profile.name.as_deref(), Some("ATTIJARIWAFA BANK"));
        assert_eq!(profile.isin.as_deref(), Some("MA0000012445"));
        assert_eq!(profile.quote_type.as_deref(), Some("EQUITY"));
        assert_eq!(profile.currency.as_deref(), Some("MAD"));
        assert_eq!(profile.exchange.as_deref(), Some("XCAS"));
        assert_eq!(profile.market_cap, Some(148_490_207_077.8));
        assert_eq!(profile.sector.as_deref(), Some("Banques"));
        assert_eq!(
            profile.website.as_deref(),
            Some("https://www.attijariwafabank.com")
        );
        assert!(profile
            .description
            .as_deref()
            .is_some_and(|d| d.starts_with("Attijariwafa Bank")));
        // Not inferred from the venue.
        assert_eq!(profile.country, None);

        let mut iam = profile_from_synthese(&rows("VALEUR-SYNTHESE", SYNTHESE_IAM), "IAM").unwrap();
        enrich_profile(&mut iam, &rows("VALEURS-INFOS", INFOS_IAM));
        assert_eq!(iam.isin.as_deref(), Some("MA0000011488"));
        assert_eq!(iam.sector.as_deref(), Some("Télécommunications"));
    }

    #[test]
    fn profile_keeps_identity_without_infos() {
        let mut profile =
            profile_from_synthese(&rows("VALEUR-SYNTHESE", SYNTHESE_CIH), "CIH").unwrap();
        enrich_profile(&mut profile, &rows("VALEURS-INFOS", INFOS_ZZZZ));
        assert_eq!(
            profile.name.as_deref(),
            Some("Crédit Immobilier et Hôtelier")
        );
        assert_eq!(profile.isin.as_deref(), Some("MA0000011454"));
        assert_eq!(profile.sector, None);
        assert_eq!(profile.website, None);
    }

    #[test]
    fn symbol_validation() {
        assert_eq!(normalize_symbol(" atw ").unwrap(), "ATW");
        assert!(normalize_symbol("").is_err());
        assert!(normalize_symbol("ATW\"}").is_err());
        assert!(normalize_symbol("../x").is_err());
        assert!(matches!(
            symbol_from_instrument(&ProviderInstrument::EquitySymbol {
                symbol: Arc::from("iam"),
            }),
            Ok(ref s) if s == "IAM"
        ));
    }
}
