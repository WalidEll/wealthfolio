-- Bourse de Casablanca (XCAS) quotes via the public CDG Capital Bourse website
-- API. The quote client only builds providers that have a row here. Disabled
-- until the user turns it on: the API is unofficial and no request is sent
-- while the row is disabled.
INSERT OR IGNORE INTO market_data_providers (id, name, description, url, priority, enabled, logo_filename, last_synced_at, last_sync_status, last_sync_error)
VALUES
    ('CDG_CAPITAL', 'CDG Capital Bourse', 'CDG Capital Bourse provides latest and end-of-day prices for shares listed on the Casablanca Stock Exchange, through its unofficial public website API. No API key required.', 'https://www.cdgcapitalbourse.ma/', 14, FALSE, 'cdg-capital.svg', NULL, NULL, NULL);
