//! Opt-in upstream smoke test; never reads portfolio data or credentials.
//!
//! About eight requests in total, all through the provider's own rate limiter;
//! no loops and no retries.
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
use rust_decimal::Decimal;
use wealthfolio_market_data::errors::MarketDataError;
use wealthfolio_market_data::{
    CdgCapitalProvider, InstrumentId, MarketDataProvider, ProviderInstrument, Quote, QuoteContext,
    QuoteIdentifiers,
};

fn context(ticker: &str) -> QuoteContext {
    QuoteContext {
        instrument: InstrumentId::Equity {
            ticker: ticker.into(),
            mic: Some("XCAS".into()),
        },
        identifiers: QuoteIdentifiers::default(),
        overrides: None,
        currency_hint: Some("MAD".into()),
        preferred_provider: None,
        bond_metadata: None,
        custom_provider_code: None,
    }
}

fn symbol(ticker: &str) -> ProviderInstrument {
    ProviderInstrument::EquitySymbol {
        symbol: ticker.into(),
    }
}

fn dates(history: &[Quote]) -> Vec<NaiveDate> {
    history.iter().map(|q| q.timestamp.date_naive()).collect()
}

fn assert_clean_daily_history(history: &[Quote]) {
    assert!(!history.is_empty(), "history is empty");
    let dates = dates(history);
    assert!(
        dates.windows(2).all(|pair| pair[0] < pair[1]),
        "bars must be oldest first with unique dates"
    );
    assert!(
        dates
            .iter()
            .all(|d| !matches!(d.weekday(), Weekday::Sat | Weekday::Sun)),
        "the Bourse de Casablanca does not trade at weekends"
    );
    for bar in history {
        assert_eq!(bar.currency, "MAD");
        assert!(bar.close > Decimal::ZERO, "bad close on {}", bar.timestamp);
        if let (Some(open), Some(high), Some(low)) = (bar.open, bar.high, bar.low) {
            assert!(
                low <= high && low <= bar.close && bar.close <= high && low <= open && open <= high,
                "OHLC does not bracket on {}: o={open} h={high} l={low} c={}",
                bar.timestamp,
                bar.close
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires live CDG Capital Bourse HTTPS access; run explicitly"]
async fn cdg_capital_search_profile_latest_and_history() {
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        let provider = CdgCapitalProvider::new();

        // Search: stocks only, no funds (ISIN symbols) or indices.
        let results = provider.search("attijari").await.unwrap();
        let atw = results
            .iter()
            .find(|r| r.symbol == "ATW")
            .expect("ATW in search results");
        assert_eq!(atw.exchange_mic.as_deref(), Some("XCAS"));
        assert_eq!(atw.currency.as_deref(), Some("MAD"));
        assert!(
            results
                .iter()
                .all(|r| r.symbol.len() <= 6 && !r.symbol.starts_with("MA0")),
            "search must not return OPCVM funds (ISIN symbols): {:?}",
            results.iter().map(|r| &r.symbol).collect::<Vec<_>>()
        );

        // Profile: two requests (VALEUR-SYNTHESE and VALEURS-INFOS).
        let profile = provider.get_profile("ATW").await.unwrap();
        assert!(profile
            .name
            .as_deref()
            .is_some_and(|n| n.to_uppercase().contains("ATTIJARI")));
        let isin = profile.isin.as_deref().expect("ISIN");
        assert_eq!(isin.len(), 12);
        assert!(isin.starts_with("MA"), "unexpected ISIN {isin}");
        assert!(profile.sector.as_deref().is_some_and(|s| !s.is_empty()));
        assert_eq!(profile.currency.as_deref(), Some("MAD"));
        assert!(
            profile.country.is_none(),
            "do not infer domicile from listing venue"
        );

        // Latest quote.
        let latest = provider
            .get_latest_quote(&context("ATW"), symbol("ATW"))
            .await
            .unwrap();
        assert_eq!(latest.currency, "MAD");
        assert!(latest.close > Decimal::ZERO);
        assert!(
            latest.close > Decimal::from(10) && latest.close < Decimal::from(10_000),
            "implausible ATW price {}",
            latest.close
        );
        assert!(
            latest.volume.is_some_and(|v| v >= Decimal::ZERO),
            "volume comes from NbrTitre"
        );
        // If DateCours is local time, the fixed +01:00 offset is exact outside
        // Ramadan and an hour early during it, so never ahead of now (small
        // clock-skew margin only). The last trade is at most a long holiday
        // stretch old. This cannot detect a UTC-based DateCours (unverified).
        let now: DateTime<Utc> = Utc::now();
        assert!(
            latest.timestamp <= now + Duration::minutes(5),
            "latest quote is in the future: {}",
            latest.timestamp
        );
        assert!(
            latest.timestamp >= now - Duration::days(10),
            "latest quote is stale: {}",
            latest.timestamp
        );
        println!(
            "ATW latest: close={} volume={:?} at {}",
            latest.close, latest.volume, latest.timestamp
        );

        // One year of history is served by VALEUR-SESSIONS alone (one request).
        let one_year_ago = now - Duration::days(365);
        let year = provider
            .get_historical_quotes(&context("ATW"), symbol("ATW"), one_year_ago, now)
            .await
            .unwrap();
        assert_clean_daily_history(&year);
        assert!(year.len() > 200, "got {} bars", year.len());
        assert!(
            year.iter().all(|q| q.open.is_some() && q.volume.is_some()),
            "sessions carry open and quantity"
        );
        let year_first = year[0].timestamp.date_naive();
        assert!(year_first <= (one_year_ago + Duration::days(10)).date_naive());
        println!(
            "ATW 1y: {} bars {} .. {}",
            year.len(),
            year_first,
            year.last().unwrap().timestamp.date_naive()
        );

        // Reaches past the ~15 months of VALEUR-SESSIONS, so VALEUR-GRAPH-APPL
        // extends it (two requests): older bars carry the close alone.
        let two_years_ago = now - Duration::days(730);
        let long = provider
            .get_historical_quotes(&context("ATW"), symbol("ATW"), two_years_ago, now)
            .await
            .unwrap();
        assert_clean_daily_history(&long);
        assert!(long.len() > year.len(), "got {} bars", long.len());
        let close_only = long.iter().filter(|q| q.open.is_none()).count();
        let with_ohlc = long.iter().filter(|q| q.open.is_some()).count();
        assert!(
            close_only > 0,
            "no close-only bars from the chart extension"
        );
        assert!(with_ohlc > 0, "no session bars");
        // Sessions win on overlap: every close-only bar precedes every session bar.
        let last_close_only = long.iter().rposition(|q| q.open.is_none()).unwrap();
        let first_session = long.iter().position(|q| q.open.is_some()).unwrap();
        assert!(
            last_close_only < first_session,
            "close-only bars must all be older than the session bars"
        );
        assert!(long
            .iter()
            .filter(|q| q.open.is_none())
            .all(|q| q.high.is_none() && q.low.is_none() && q.volume.is_none()));
        // Session bars from the long request match the one-year request.
        for bar in &year {
            let same = long
                .iter()
                .find(|q| q.timestamp.date_naive() == bar.timestamp.date_naive())
                .expect("session bar present in the long history");
            assert_eq!(same.close, bar.close);
            assert_eq!(same.open, bar.open);
        }
        println!(
            "ATW 2y: {} bars ({} close-only, {} sessions) {} .. {}",
            long.len(),
            close_only,
            with_ohlc,
            long[0].timestamp.date_naive(),
            long.last().unwrap().timestamp.date_naive()
        );

        // Unknown symbol.
        let error = provider
            .get_latest_quote(&context("ZZZZ"), symbol("ZZZZ"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, MarketDataError::SymbolNotFound(_)),
            "expected not found, got {error:?}"
        );
    })
    .await
    .expect("CDG Capital live test timed out");
}
