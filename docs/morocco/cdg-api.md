# CDG Capital Bourse public API — reverse-engineered notes

Source: the public website https://www.cdgcapitalbourse.ma (React SPA, bundle
`static/js/main.<hash>.js`). No login, no API key. Observed 2026-11-09.

> Undocumented, unofficial API. It can change without notice. Keep requests
> low-volume (EOD use only), identify politely, and check CDG Capital Bourse's
> terms of use before distributing a build that calls it.

## Transport

- `POST https://www.cdgcapitalbourse.ma/api/`
- `Content-Type: application/json`
- One endpoint for everything. The body is an RPC envelope; several actions can
  be batched in one call.

```json
{
  "ACTIONS": [
    {
      "ACTION": {
        "NAME": "VALEUR-SYNTHESE",
        "TYPE": "SELECT",
        "VALUE": "VALEUR-SYNTHESE"
      },
      "PARAMS": [
        { "NAME": "Lang_", "TYPE": "S", "VALUE": "fr" },
        { "NAME": "Espace_", "TYPE": "I", "VALUE": "1" },
        { "NAME": "Symbol_", "TYPE": "S", "VALUE": "ATW" },
        { "NAME": "IdPartener_", "TYPE": "I", "VALUE": "1" }
      ]
    }
  ]
}
```

Response: a JSON array, one object per action, keyed by action name:

```json
[ { "VALEUR-SYNTHESE": { "Valid": true, "Data": [ ... ] } } ]
```

`TYPE` is `S` (string) or `I` (integer, still sent as a string). `Lang_` is `fr`
/ `en` / `ar`. `Espace_` and `IdPartener_` are always `"1"` on the site.

## Actions relevant to the adapter

| Need                           | Action                                  | Params (besides Lang_/Espace_)                                                                            | Status                       |
| ------------------------------ | --------------------------------------- | --------------------------------------------------------------------------------------------------------- | ---------------------------- |
| Latest quote                   | `VALEUR-SYNTHESE`                       | `Symbol_`, `IdPartener_`                                                                                  | **captured**                 |
| EOD history                    | `VALEUR-SESSIONS`                       | `Symbol_`, `Top_` (row count)                                                                             | **captured** (max 306 rows)  |
| Price chart (fallback history) | `VALEUR-GRAPH-APPL`                     | `Symbol_`                                                                                                 | **captured** (about 3 years) |
| Intraday                       | `VALEUR-INTRA`                          | `Symbol_` (assumed)                                                                                       | optional                     |
| Search                         | `QUICK-SEARCH`                          | `IdPartener_`, `Query_`                                                                                   | **captured**                 |
| Profile                        | `VALEURS-INFOS`                         | `Symbol_`                                                                                                 | **captured**                 |
| All listed stocks + last price | `PALMARES-STOCKS`                       | `TypeStocks_`=`1` (I), `IdPartener_`, `TypeOrder_` (`H`/`B`/`volume`), `Frequence_`=`D`, `Nbr_` (ignored) | **captured** (below)         |
| Index history (MASI, MSI20)    | `INDICE-SESSIONS` / `INDICE-GRAPH-HIST` | `Indice_`, `Top_` / `Frequence_`                                                                          | phase 2                      |
| Market open/closed             | `MARKET-STATUS`                         | `IdPartener_`, `NumSeq_`                                                                                  | optional                     |
| OPCVM NAVs                     | `OPCVM-GRAPH`, `OPCVM-SYNTHESE`         | `IdValeur_`                                                                                               | out of scope (phase 2)       |

Other actions seen in the bundle (not needed): `INDICE-STOCKS`,
`INDICE-O-GRAPH-INTRA`, `INDICE-SYNTHESE`, `INDICE-COMPOSITION`,
`INDICE-INDICATORS`, `INDICE-INTERNATIONAL`, `VALEUR-PROFONDEUR`,
`VALEUR-INDICATORS`, `VALEUR-TRANSACTION`, `VALEUR-MARKET-INDICATOR`,
`VALEUR-PUBLICATIONS`, `VALEUR-FINANCIALS`, `VALEURS-REPARTITION`,
`VALEURS-DIRIGEANTS`, `AGENDA-ALL`, `PUBLICATION-ALL`, `MARKET-RESUME`. Never
call `ACCOUNT-CREATE` / `FAMILY-SITUATION` (account-opening flow).

## Captured shape — `PALMARES-STOCKS` row

```json
{
  "Symbol": "ATW",
  "Libelle": "ATTIJARIWAFA BANK",
  "IdTypeValeur": 1,
  "CoursDeReferance": 694.6,
  "DernierCours": 690.2,
  "Ouverture": 693.9,
  "DateDernierCours": "09/11/2026 13:48:14",
  "Variation": -0.63,
  "QteEchangee": 20472,
  "Volume": 14181540.9,
  "PlusHaut": 694,
  "PlusBas": 682,
  "Statut": "T",
  "Capitalisation": 148490207077.8,
  "Type": "Action",
  "DataChart": "680.00;676.10;...;694.60|G"
}
```

Field meaning:

- `DernierCours` last trade, `CoursDeReferance` previous close (reference),
  `Ouverture` open, `PlusHaut`/`PlusBas` high/low, `QteEchangee` shares traded,
  `Volume` traded value **in MAD** (not share count).
- `DateDernierCours` is `dd/MM/yyyy HH:mm:ss`, Casablanca local time.
- `DataChart` = last ~30 closes separated by `;`, then `|G` / `|R` (colour).
  Usable as a coarse history fallback only.
- Numbers are JSON numbers; prices in MAD.
- Symbols are the Bourse de Casablanca tickers (ATW, IAM, BCP, CIH, …).

## Conventions common to every action

- Fixtures:
  `crates/market-data/src/provider/cdg_capital/fixtures/<ACTION>_<symbol>.json`
  (pretty-printed, trimmed to a few rows; `Presentation` text truncated).
- HTTP status is always `200`, even for bad input. Request with a clear
  `User-Agent`; no cookies or auth were needed or stored.
- Dates are `dd/MM/yyyy HH:mm:ss` (day first), no time zone, Casablanca local
  time. Daily bars use `00:00:00` as the time. Parse with the format, never with
  a locale parser.
- Numbers are JSON numbers (int or float), decimal point, no thousands
  separators, MAD. An unavailable numeric/date field is the empty string `""`,
  not `null` (for example `VariationV`, `PTO`, `VTO`, `CoursDAnnee`, `NomLogo`,
  and the index/OPCVM rows of `QUICK-SEARCH`). Deserialize numerics tolerantly
  (number or `""`).
- Symbol lookup is case-insensitive (`atw` returns ATW).
- Envelope response: `[ { "<ACTION>": { "Valid": bool, "Data": ... } } ]`.

### Error and not-found behaviour

| Case                                                                 | Response                                                         |
| -------------------------------------------------------------------- | ---------------------------------------------------------------- |
| Unknown symbol (`ZZZZ`), any action                                  | `Valid:true`, `Data:[]` (HTTP 200)                               |
| Search with no match                                                 | `Valid:true`, `Data:[]`                                          |
| Bad parameter (e.g. non-integer `TypeStocks_`)                       | `Valid:false`, `Data:"Erreur Système."` (a string, not an array) |
| `PALMARES-STOCKS` with `TypeOrder_=volume` (the value the site uses) | `Valid:true`, `Data:[]`                                          |

So treat "empty `Data`" as not found, and `Valid:false` as an upstream error
(retryable/circuit-breaker). `Data` can be a string when `Valid` is false. An
"empty history" for a real symbol was not reachable (all listed symbols have
history); the unknown symbol case (`VALEUR-SESSIONS_ZZZZ.json`,
`VALEUR-GRAPH-APPL_ZZZZ.json`) is the empty case.

## Captured shape: `VALEUR-SYNTHESE` (latest quote)

Fixtures: `VALEUR-SYNTHESE_{ATW,IAM,CIH,ZZZZ}.json`. `Data` has one object.

| Field                              | Type       | Meaning                                                                                                                                                                                            |
| ---------------------------------- | ---------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Symbol`, `Libelle`                | string     | ticker, display name (case varies: "ATTIJARIWAFA BANK", "Crédit Immobilier et Hôtelier")                                                                                                           |
| `Cours`                            | number     | last trade price                                                                                                                                                                                   |
| `DateCours`                        | string     | time of last trade                                                                                                                                                                                 |
| `Ising`                            | string     | **ISIN** (field name is misspelled upstream), e.g. `MA0000012445`                                                                                                                                  |
| `Cotation`, `Phase`                | string     | `Active`, `Regular Trading` (market phase)                                                                                                                                                         |
| `VariationP`                       | number     | % change vs `CoursVeille`                                                                                                                                                                          |
| `VariationV`                       | `""`       | usually empty                                                                                                                                                                                      |
| `Ouverture`, `PlusHaut`, `PlusBas` | number     | day open/high/low                                                                                                                                                                                  |
| `SeuilHaut`, `SeuilBas`            | number     | daily price limits                                                                                                                                                                                 |
| `CMP`                              | number     | volume-weighted average price of the day                                                                                                                                                           |
| `NbrTitre`                         | int        | **shares traded today (quantity)**                                                                                                                                                                 |
| `NbrTransaction`                   | int        | number of trades                                                                                                                                                                                   |
| `Volume`                           | number     | **traded value in MAD** (CMP x NbrTitre), not quantity                                                                                                                                             |
| `PourcentageAchat/Vente`           | int        | order book split                                                                                                                                                                                   |
| `CoursVeille`                      | number     | previous close                                                                                                                                                                                     |
| `CoursDAnnee`, `DateCoursDAnnee`   | `""`, date | year-start reference (31/12 of previous year); `VariationPDAnnee`/`VariationVPDAnnee` are YTD absolute (MAD) / percent (CIH: price 310, PDAnnee -105, year-start 415, -105/415 = -25.3 = VPDAnnee) |
| `PlusHautDAnnee`, `PlusBasDAnnee`  | number     | YTD high / low                                                                                                                                                                                     |
| `Capitalisation`                   | number     | market cap in MAD                                                                                                                                                                                  |
| `DateJour`                         | string     | server timestamp of the snapshot                                                                                                                                                                   |

Note: quantity here is `NbrTitre`; the equivalent in PALMARES is `QteEchangee`,
in VALEUR-SESSIONS it is `Qte`, and in QUICK-SEARCH rows `NbrEchange` is the
same share quantity (ATW 20845 = `NbrTitre` at the same timestamp).

## Captured shape: `VALEUR-SESSIONS` (EOD history)

Fixtures: `VALEUR-SESSIONS_{ATW,IAM,CIH,ZZZZ}.json` (5 rows each).

```json
{
  "Cours": 694.6,
  "DateCours": "05/11/2026 00:00:00",
  "Ouverture": 670,
  "PlusHaut": 694.6,
  "PlusBas": 670,
  "Qte": 34194
}
```

- Fields: `Cours` close, `Ouverture` open, `PlusHaut`, `PlusBas`, `Qte` **share
  quantity** (no traded value). No symbol field in rows.
- Ordering: newest first. Dates unique, one row per trading day.
- Today's session is **not** included until the day is finalised (call on
  2026-11-09 during trading returned 05/11 as newest; 06/11 was a holiday, and
  it was still missing at 16:41 local). `VALEUR-SYNTHESE` has the current day,
  but Wealthfolio stores quotes only through history sync and never saves a
  latest-quote result, so the adapter does not store same-day prices. When the
  day appears in `VALEUR-SESSIONS` is unmeasured.
- `Top_` limits: `250` honoured (250 rows back to 29/10/2025); `1000` and `5000`
  both return **306 rows** (06/08/2025 to 05/11/2026), so the server caps
  history at about 15 months / 306 sessions. Use `Top_=1000` for "all
  available"; longer history needs `VALEUR-GRAPH-APPL` (about 3 years).
- Cheap incremental fetch: `Top_` = number of sessions missing + a few.

## Captured shape: `VALEUR-GRAPH-APPL` (long chart history)

Fixtures: `VALEUR-GRAPH-APPL_{ATW,ZZZZ}.json` (4 of 735 rows kept).

```json
{
  "DateCotation": "10/11/2023 00:00:00",
  "Ouverture": 457.5,
  "PlusHaut": 457.5,
  "PlusBas": 457.35,
  "Cloture": 458,
  "Volume": 44874354.5
}
```

- 735 rows for ATW (10/11/2023 to 05/11/2026, about 3 years), **oldest first**,
  about 89 KB. No `Top_`.
- `Volume` is traded value in MAD (no quantity).
- Caveat: Open/High/Low look unreliable (often equal to each other and not
  bracketing `Cloture`; e.g. 05/11/2026 shows O=H=L=670, C=694.6 while
  `VALEUR-SESSIONS` has H=694.6). Use only `Cloture` (and date) from this
  action, and prefer `VALEUR-SESSIONS` OHLC where available.

## Captured shape: `QUICK-SEARCH`

Fixtures: `QUICK-SEARCH_{attijari,cih,bank,ZZZZ}.json`.

```json
{
  "Id": "",
  "Symbol": "ATW",
  "Libelle": "ATTIJARIWAFA BANK",
  "DateCotation": "09/11/2026 14:36:47",
  "DernierCours": 690.2,
  "Variation": -0.63,
  "Premier": 693.9,
  "PlusHaut": 694,
  "PlusBas": 682,
  "NbrEchange": 20845,
  "type": "A"
}
```

- Substring match on name and symbol ("bank" returns ATW, BOA, CFG, and the
  index BANK; "cih" returns CIH plus many FCP funds).
- `type`: `A` stocks, `O` OPCVM funds (`Symbol` is the fund **ISIN**, `Id`
  numeric, price fields `""`, `DateCotation` is a NAV date, can be in the future
  relative to today), `I` indices (e.g. `BANK`, `Symbol` is an index code).
  **Filter to `type == "A"`** for the adapter.
- No ISIN for stocks in search (get it from `VALEUR-SYNTHESE.Ising`); no
  currency (always MAD); no exchange field (always XCAS). Name matching on the
  Latin names only; no ranking field.

## Captured shape: `VALEURS-INFOS` (profile)

Fixtures: `VALEURS-INFOS_{ATW,IAM,CIH,ZZZZ}.json`.

Fields: `Presentation` (French company description, long), `Mnemonique`
(ticker), `DateIntroduction` (listing date), `SiteInternet` (no scheme),
`NomLogo` (empty so far), `DateConstitution` (incorporation date),
`DateDetachementDividende` (latest ex-dividend date), `CapitalSocial` (share
capital, MAD), `ValeurNominal` (par value), `Secteur` (French sector name, e.g.
"Banques", "Télécommunications"). Do not derive a share count from
`CapitalSocial / ValeurNominal`: it matches `Capitalisation` for ATW and IAM but
is about 36% off for CIH (`CapitalSocial` can be stale). Use `Capitalisation`
from `VALEUR-SYNTHESE`.

No ISIN here and no share-count field; ISIN comes from `VALEUR-SYNTHESE`.
Language follows `Lang_`; descriptions were requested with `fr` only.

## Captured shape: `PALMARES-STOCKS` parameters

Not used by the adapter (no fallback is built on it). The site calls it with
`TypeStocks_=1` (integer), `Frequence_=D`, `TypeOrder_` in `H` (gainers), `B`
(losers), `volume` (the latter returned an empty list for us; do not rely on it)
and `Nbr_` which is **ignored**: one call returned 39 rows (about 20 KB).
Unverified: whether `H` returns every listed equity or only gainers (all three
saved fixture rows have a positive `Variation` and are in symbol order, not
variation order), so do not assume it covers stocks that fell. All rows had
`Type:"Action"`, `Statut:"T"`. Fixture: `PALMARES-STOCKS_error.json`
(`Valid:false`).

## Field mapping to Wealthfolio

### `Quote` (`crates/market-data/src/models/quote.rs`)

| Quote field    | Latest (`VALEUR-SYNTHESE`) | EOD (`VALEUR-SESSIONS`) | Fallback (`VALEUR-GRAPH-APPL`)             |
| -------------- | -------------------------- | ----------------------- | ------------------------------------------ |
| `close`        | `Cours`                    | `Cours`                 | `Cloture`                                  |
| `open`         | `Ouverture`                | `Ouverture`             | omit (unreliable)                          |
| `high` / `low` | `PlusHaut` / `PlusBas`     | `PlusHaut` / `PlusBas`  | omit (unreliable)                          |
| `volume`       | `NbrTitre` (quantity)      | `Qte`                   | omit (API `Volume` is value, not quantity) |
| `currency`     | `"MAD"` constant           | `"MAD"`                 | `"MAD"`                                    |
| `timestamp`    | `DateCours`                | `DateCours` date        | `DateCotation` date                        |
| `source`       | `CDG_CAPITAL`              | same                    | same                                       |

- Prices are already in MAD units (not minor units); no scaling.
- Timestamps: parse `dd/MM/yyyy HH:mm:ss` as Africa/Casablanca. Daily bars: take
  the date and stamp at session close in a fixed `+01:00` offset (date stays
  correct). Latest quote: use `DateCours` as-is with `+01:00` (off by one hour
  during Ramadan, UTC+0). Session close hours: the site showed
  `Phase: Regular Trading` at 14:56 local; the 15:30 close in the plan is
  **unverified** (no capture after 15:00, normal exchange hours are continuous
  trading to about 15:20 and closing auction to about 15:30).
- Latest during a closed day: `DateCours` stays at the last trade (may be a
  prior day); do not treat as an error.

### `SearchResult` (`models/search.rs`)

`symbol` = `Symbol`, `name` = `Libelle`, `exchange_mic` = `XCAS`,
`exchange_name` = `Casablanca`, `currency` = `MAD`, `asset_type` = equity (only
`type == "A"`), `data_source` = `CDG_CAPITAL`. No score upstream.

### `AssetProfile` (`models/profile.rs`)

`name` = `Libelle` (from SYNTHESE or search), `isin` = `Ising`, `sector` =
`Secteur`, `website` = `https://` + `SiteInternet`, `description` =
`Presentation`, `currency` = `MAD`, `exchange` = `XCAS`, `country` = `MA`,
`market_cap` = `Capitalisation` (SYNTHESE), `week_52_high/low` not available
(only YTD high/low), `quote_type` = equity. Profile therefore needs two calls
(VALEURS-INFOS + VALEUR-SYNTHESE) if ISIN/market cap are wanted.

## Open questions

- Exact session close time and the Ramadan schedule (no after-close capture).
- Whether `DateCours` is Casablanca local time or UTC. On 2026-11-09 the server
  `DateJour` read 15:41:25, equal to the UTC wall clock (HTTP `Date` 15:41:43
  GMT; local was 16:41), while ATW's last trade read 15:18:02. A local
  `DateCours` fits a continuous session ending about 15:20; a UTC one would put
  the trade at 16:18 local, after the close. To settle it, capture
  `VALEUR-SYNTHESE` just after the 09:30 open and compare the first trade time.
  Bar dates are unaffected either way.
- Evidence needed before any same-day-price feature: `VALEUR-SESSIONS` at about
  17:00, 19:00 and 22:00 local and the next morning (when day D appears);
  `VALEUR-SYNTHESE` `Cours` after the close versus the published session close;
  and `VALEUR-SYNTHESE` for a stock that did not trade that day.
- Whether `VALEUR-SESSIONS` adds the current day after close (re-check after
  16:00 local).
- Whether `Lang_=en/ar` changes `Libelle`/`Secteur` (not probed).
- Terms of use of CDG Capital Bourse for programmatic access.
- 306-row cap may be a date window (about 15 months) rather than a row count.
