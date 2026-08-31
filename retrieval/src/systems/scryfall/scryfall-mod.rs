mod bulk;
mod parsing;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eyre::OptionExt;
use models::{
    Card, CardID, CardIdentifiers, CardPrices, CollectorNumber, RetailerPrices, SetCode,
    filters::{CardSearchFilters, SortField, SortOrder},
};
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{NamedRetrievalSystem, RetrievalSystemTrait};

/// How long a fetched card stays cached before we'd fetch it again. Card
/// data (name, oracle text, printings, artwork) is effectively static once
/// printed, so this is generous by design -- it exists mainly to eventually
/// pick up rare corrections, not because the data actually changes often.
/// (Prices are fetched through a separate endpoint and aren't affected by
/// this cache.) Only used for the live-API fallback path -- most lookups
/// are served from the bulk database below when one is configured.
const CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, Default)]
pub struct ScryfallRetrievalSystem {
    /// Shared across every clone of this system (all clones point at the same
    /// server-startup instance) -- see `clone_retrieval_systems_by_name` in
    /// the server crate. This is what makes the cache actually persist across
    /// requests instead of starting empty every time.
    ///
    /// This exists because collection search fires the same card lookups
    /// repeatedly in quick succession -- a "search" request and a "count"
    /// request run concurrently for every search, and searching re-fires as
    /// you type -- which was re-fetching a whole collection's worth of cards
    /// from Scryfall over and over and tripping the 10 req/sec rate limit
    /// even with batched requests. Caching means only the first fetch after
    /// a cold start actually hits the network; everything after that is
    /// served from memory.
    cache: Arc<Mutex<HashMap<String, (Instant, Card)>>>,

    /// Path to a locally-downloaded snapshot of Scryfall's bulk card data
    /// (see `bulk.rs`). When set, searches and lookups query this local
    /// database first and only fall back to the live API for whatever it
    /// doesn't have (e.g. a card newer than the last daily refresh, or a
    /// search using a filter the bulk query doesn't support). `None`
    /// disables bulk mode entirely -- every query goes straight to the live
    /// API via the cache above, the original behaviour.
    bulk_db_path: Option<String>,

    /// The actual open connection to the bulk database, behind a lock so the
    /// background refresh task can swap in a freshly rebuilt database
    /// without any in-flight query ever seeing a half-written one.
    bulk_conn: Arc<Mutex<Option<Connection>>>,
}

impl NamedRetrievalSystem for ScryfallRetrievalSystem {
    fn name(&self) -> &str {
        "Scryfall"
    }
}

impl ScryfallRetrievalSystem {
    /// `bulk_db_path`: when `Some`, enables bulk mode -- an existing database
    /// at that path is loaded immediately (even if stale, so the system is
    /// usable right away), and a background task keeps it refreshed roughly
    /// daily for the lifetime of the process. Pass `None` to keep the
    /// original always-live-API behaviour.
    pub fn new(bulk_db_path: Option<String>) -> eyre::Result<Self> {
        let initial_conn: Option<Connection> = match &bulk_db_path {
            Some(path) if std::path::Path::new(path).exists() => bulk::open_bulk_db(path).ok(),
            _ => None,
        };
        let bulk_conn = Arc::new(Mutex::new(initial_conn));

        let system = Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
            bulk_db_path: bulk_db_path.clone(),
            bulk_conn: bulk_conn.clone(),
        };

        if let Some(path) = bulk_db_path {
            tokio::spawn(async move {
                loop {
                    match bulk::ensure_fresh(&path).await {
                        Ok(Some(new_conn)) => {
                            tracing::info!(
                                "Scryfall bulk: local database refreshed and swapped in"
                            );
                            *bulk_conn.lock().await = Some(new_conn);
                        }
                        Ok(None) => {
                            // Already fresh, nothing to do this round.
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Scryfall bulk: failed to refresh local database, \
                                 will retry in {} minutes: {e}",
                                bulk::CHECK_INTERVAL_SECS / 60
                            );
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(bulk::CHECK_INTERVAL_SECS)).await;
                }
            });
        }

        Ok(system)
    }

    /// Whether `filters` only uses fields the bulk database can query
    /// directly (name, set code, collector number, all_printings). Anything
    /// else (color, type, text, rarity, mana value, ...) isn't indexed in
    /// the local database, so those searches fall back to the live API
    /// where Scryfall's full query syntax handles them correctly.
    fn bulk_supports_filters(filters: &CardSearchFilters) -> bool {
        filters.color_identities.is_none()
            && filters.artist.is_none()
            && filters.text.is_none()
            && filters.rarity.is_none()
            && filters.subtypes.is_none()
            && filters.supertypes.is_none()
            && filters.types.is_none()
            && filters.mana_value_min.is_none()
            && filters.mana_value_max.is_none()
            && filters.colors.is_none()
            && filters.keywords.is_none()
            && filters.power.is_none()
            && filters.toughness.is_none()
            && filters.loyalty.is_none()
            && filters.defense.is_none()
            && filters.is_reserved.is_none()
            && filters.is_promo.is_none()
            && filters.is_reprint.is_none()
            && filters.is_full_art.is_none()
            && filters.border_color.is_none()
            && filters.legal_in.is_none()
    }
}

/// Returns the first entry of `card_faces` for a raw Scryfall JSON object, if
/// present. Multi-faced cards (MDFCs, transform, split, adventure) omit
/// several gameplay fields from the top-level object -- Scryfall nests them
/// per-face instead -- so this is used as a fallback source for those fields.
fn front_face(json: &Value) -> Option<&serde_json::Map<String, Value>> {
    json.get("card_faces")
        .and_then(Value::as_array)
        .and_then(|faces| faces.first())
        .and_then(Value::as_object)
}

/// Reads a string field from the top-level card JSON, falling back to the
/// same key on the first card face if the top-level field is absent.
fn str_field_with_face_fallback<'a>(
    json: &'a Value,
    face: Option<&'a serde_json::Map<String, Value>>,
    key: &str,
) -> Option<&'a str> {
    json.get(key)
        .and_then(Value::as_str)
        .or_else(|| face.and_then(|f| f.get(key)).and_then(Value::as_str))
}

impl ScryfallRetrievalSystem {
    /// Fetches a batch of up to 75 cards by ID using Scryfall's bulk
    /// "collection" endpoint (a single POST request), instead of one GET
    /// request per card. Fetching one at a time doesn't scale: a collection
    /// of a few hundred cards would send that many requests back-to-back and
    /// blow straight through Scryfall's 10 req/sec rate limit (this is what
    /// produced the "You are being rate-limited" warnings in the logs).
    /// Batching keeps even a large collection to a handful of requests.
    /// Performs the actual POST to Scryfall's bulk "collection" endpoint and
    /// returns the raw `data` array of card JSON objects, keyed by id.
    /// Shared by `fetch_cards_batch` (parses into `Card`) and the prices
    /// path (reads the `prices` object directly, which parsing discards).
    async fn fetch_batch_raw(ids: &[String]) -> eyre::Result<HashMap<String, Value>> {
        let identifiers: Vec<Value> = ids
            .iter()
            .map(|id| serde_json::json!({ "id": id }))
            .collect();

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("gathers_cli/1.0"),
        );
        headers.insert("Accept", reqwest::header::HeaderValue::from_static("*/*"));
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        let client = reqwest::Client::new();
        let response = client
            .post("https://api.scryfall.com/cards/collection")
            .headers(headers)
            .json(&serde_json::json!({ "identifiers": identifiers }))
            .send()
            .await?;
        let json: Value = response.json().await?;

        if let Some(error) = json.get("object").and_then(Value::as_str)
            && error == "error"
        {
            let error_msg = json
                .get("details")
                .and_then(Value::as_str)
                .unwrap_or("Unknown error");
            return Err(eyre::eyre!("Scryfall API error: {}", error_msg));
        }

        if let Some(not_found) = json.get("not_found").and_then(Value::as_array)
            && !not_found.is_empty()
        {
            tracing::warn!(
                "Scryfall: {} card id(s) in this batch were not found on Scryfall and will be missing from results",
                not_found.len()
            );
        }

        let cards_array = json
            .get("data")
            .and_then(Value::as_array)
            .ok_or_eyre("Could not retrieve cards array")?;

        let mut result = HashMap::new();
        for raw in cards_array {
            let Some(obj) = raw.as_object() else { continue };
            let Some(id) = obj.get("id").and_then(Value::as_str) else {
                continue;
            };
            result.insert(id.to_string(), Value::Object(obj.clone()));
        }
        Ok(result)
    }

    async fn fetch_cards_batch(ids: &[String]) -> eyre::Result<HashMap<String, models::Card>> {
        let raw = Self::fetch_batch_raw(ids).await?;
        let mut result = HashMap::new();
        for (id, value) in &raw {
            let Some(obj) = value.as_object() else { continue };
            if let Some(card) = parsing::parse_card(obj) {
                result.insert(id.clone(), card);
            } else {
                tracing::warn!(
                    "Scryfall: failed to parse card {id} from a batch response, skipping it"
                );
            }
        }
        Ok(result)
    }

    /// Parses Scryfall's `prices` object (present on every card object) into
    /// our `CardPrices` model. Scryfall represents each price as a JSON
    /// string (e.g. `"0.25"`) or `null`, not a number. Only USD is used --
    /// the UI's price display hardcodes a `$` prefix, so mixing in EUR
    /// figures would show wrong/misleading numbers.
    fn extract_prices(id: &str, card_json: &Value) -> Option<CardPrices> {
        let prices = card_json.get("prices")?;
        let usd = prices
            .get("usd")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<f64>().ok());
        let usd_foil = prices
            .get("usd_foil")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<f64>().ok());

        if usd.is_none() && usd_foil.is_none() {
            return None;
        }

        let mut paper = HashMap::new();
        paper.insert(
            "scryfall".to_string(),
            RetailerPrices {
                normal: usd,
                foil: usd_foil,
            },
        );

        Some(CardPrices {
            uuid: id.to_string(),
            paper,
        })
    }
}

impl RetrievalSystemTrait for ScryfallRetrievalSystem {
    #[allow(unused_variables)]
    async fn search_cards(
        &self,
        filters: CardSearchFilters,
        skip: Option<usize>,
        limit: Option<usize>,
    ) -> eyre::Result<Vec<Card>> {
        // The bulk database only has fast, indexed support for name/set/
        // collector-number lookups. Anything using a filter beyond that
        // (color, type, text, rarity, ...) falls straight through to the
        // live API below, which already supports Scryfall's full query
        // syntax -- this keeps every filter fully correct while still
        // routing the overwhelmingly common case (name search, which is
        // what collection filtering and the printing picker both use)
        // through the local database with zero network calls.
        if Self::bulk_supports_filters(&filters) {
            let bulk = self.bulk_conn.lock().await;
            if let Some(conn) = bulk.as_ref() {
                let exact_name = filters.all_printings == Some(true);
                let rows = bulk::search(
                    conn,
                    filters.name.as_deref(),
                    exact_name,
                    filters.set_code.as_deref(),
                    filters.collector_number.as_deref(),
                    limit.unwrap_or(175) as i64,
                    skip.unwrap_or(0) as i64,
                )?;
                return Ok(rows
                    .iter()
                    .filter_map(|v| v.as_object().and_then(parsing::parse_card))
                    .collect());
            }
        }

        let mut query = vec![];

        // An exact name match is required for `all_printings` to make sense:
        // fuzzy/partial name matches would otherwise pull in every printing of
        // every card whose name merely contains the search text.
        if filters.all_printings == Some(true)
            && let Some(name) = &filters.name
        {
            query.push(format!("!\"{}\"", name.replace('"', "")));
        } else if let Some(name) = &filters.name {
            query.push(format!("name:{}", name));
        }

        if let Some(set_code) = &filters.set_code {
            query.push(format!("set:{}", set_code));
        }

        if let Some(color_identities) = &filters.color_identities {
            for color in color_identities {
                query.push(format!("c:{}", color));
            }
        }

        if let Some(text) = &filters.text {
            query.push(format!("t:{}", text));
        }

        if let Some(types) = &filters.types {
            for t in types {
                query.push(format!("type:{}", t));
            }
        }

        if let Some(subtypes) = &filters.subtypes {
            for s in subtypes {
                query.push(format!("type:{}", s));
            }
        }

        if let Some(supertypes) = &filters.supertypes {
            query.push(format!("type:{}", supertypes));
        }

        let query_string = query.join(" ");

        let page = skip.map(|s| s / 100).unwrap_or(1);
        let unique = if filters.all_printings == Some(true) { "prints" } else { "cards" };
        let order = match &filters.sort_by {
            Some(SortField::Rarity) => "rarity",
            Some(SortField::SetCode) => "set",
            Some(SortField::CollectorNumber) => "collector_number",
            Some(SortField::Artist) => "artist",
            // Default to newest-first release order for an "all printings" lookup
            // rather than "name" (which is meaningless when every result shares
            // the same name) so recent/common printings surface first.
            None if filters.all_printings == Some(true) => "released",
            _ => "name",
        };
        let dir = if matches!(&filters.sort_order, Some(SortOrder::Desc)) {
            "desc"
        } else {
            "asc"
        };
        let include_extras = false;

        let url = format!(
            "https://api.scryfall.com/cards/search?q={}&page={}&unique={}&order={}&dir={}&include_extras={}",
            query_string, page, unique, order, dir, include_extras
        );

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("gathers_cli/1.0"),
        );
        headers.insert("Accept", reqwest::header::HeaderValue::from_static("*/*"));
        let client = reqwest::Client::new();
        let response = client.get(url).headers(headers).send().await?;
        let json: Value = response.json().await?;

        if let Some(error) = json.get("object").and_then(Value::as_str)
            && error == "error"
        {
            let error_msg = json
                .get("details")
                .and_then(Value::as_str)
                .unwrap_or("Unknown error");
            return Err(eyre::eyre!("Scryfall API error: {}", error_msg));
        }

        let cards_array = json
            .get("data")
            .and_then(Value::as_array)
            .ok_or_eyre("Could not retrieve cards array")?;

        let limit = limit.unwrap_or(cards_array.len());
        let cards = cards_array
            .iter()
            .take(limit)
            .filter_map(|card| parsing::parse_card(card.as_object()?))
            .collect::<Vec<Card>>();

        Ok(cards)
    }

    async fn get_cards_by_ids(
        &self,
        ids: Vec<String>,
    ) -> eyre::Result<HashMap<String, models::Card>> {
        let mut result = HashMap::new();
        let mut remaining = ids;

        // Bulk database first: local lookups, zero network calls, and this
        // is the path collection search hits on every request -- so this is
        // what actually stops the repeated re-fetching that was tripping
        // Scryfall's rate limit.
        {
            let bulk = self.bulk_conn.lock().await;
            if let Some(conn) = bulk.as_ref() {
                let mut still_missing = Vec::new();
                for id in remaining {
                    match bulk::get_card(conn, &id) {
                        Ok(Some(raw)) => {
                            if let Some(card) = raw.as_object().and_then(parsing::parse_card) {
                                result.insert(id, card);
                            } else {
                                still_missing.push(id);
                            }
                        }
                        _ => still_missing.push(id),
                    }
                }
                remaining = still_missing;
            }
        }

        if remaining.is_empty() {
            return Ok(result);
        }

        // Anything the bulk snapshot didn't have (e.g. a card newer than the
        // last daily refresh, or bulk mode isn't configured at all) falls
        // through to the existing cache + live-batch-fetch path.
        let mut to_fetch: Vec<String> = Vec::new();

        // Serve whatever we can from cache first -- this is what actually
        // fixes the rate-limit problem: repeated lookups for the same cards
        // (search + count firing concurrently, re-searching as you type)
        // stop hitting the network entirely after the first successful fetch.
        {
            let cache = self.cache.lock().await;
            let now = Instant::now();
            for id in &remaining {
                match cache.get(id) {
                    Some((cached_at, card)) if now.duration_since(*cached_at) < CACHE_TTL => {
                        result.insert(id.clone(), card.clone());
                    }
                    _ => to_fetch.push(id.clone()),
                }
            }
        }

        if to_fetch.is_empty() {
            return Ok(result);
        }

        // Scryfall's bulk "collection" endpoint accepts up to 75 identifiers
        // per request. Fetching one card per request (the old approach) sends
        // as many requests as there are cards in the collection, which blows
        // through Scryfall's 10 req/sec limit for anything but small
        // collections. Chunking into batches of 75 keeps even a
        // several-hundred-card collection down to a handful of requests.
        let chunks: Vec<&[String]> = to_fetch.chunks(75).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            if i > 0 {
                // A small pause between batches. Comfortably under the 10
                // req/sec limit even if other requests are happening
                // concurrently (e.g. a search running at the same time).
                tokio::time::sleep(Duration::from_millis(150)).await;
            }

            // A whole batch failing (network blip, transient 5xx, rate limit,
            // ...) must not take down every other batch -- skip it and keep
            // going so one bad batch doesn't wipe out the rest of the
            // collection.
            match Self::fetch_cards_batch(chunk).await {
                Ok(batch) => {
                    {
                        let mut cache = self.cache.lock().await;
                        let now = Instant::now();
                        for (id, card) in &batch {
                            cache.insert(id.clone(), (now, card.clone()));
                        }
                    }
                    result.extend(batch);
                }
                Err(e) => {
                    tracing::warn!(
                        "Scryfall: failed to fetch a batch of {} card(s), skipping them: {e}",
                        chunk.len()
                    );
                }
            }
        }

        Ok(result)
    }

    async fn get_sets(&self) -> eyre::Result<Vec<models::Set>> {
        // TODO: implement this
        Ok(vec![])
    }

    async fn get_random_card(&self) -> eyre::Result<Option<models::Card>> {
        {
            let bulk = self.bulk_conn.lock().await;
            if let Some(conn) = bulk.as_ref()
                && let Some(raw) = bulk::get_random_card(conn)?
            {
                return Ok(raw.as_object().and_then(parsing::parse_card));
            }
        }

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("gathers_cli/1.0"),
        );
        headers.insert("Accept", reqwest::header::HeaderValue::from_static("*/*"));
        let client = reqwest::Client::new();
        let response = client
            .get("https://api.scryfall.com/cards/random")
            .headers(headers)
            .send()
            .await?;
        let json: Value = response.json().await?;

        if let Some(error) = json.get("object").and_then(Value::as_str)
            && error == "error"
        {
            let error_msg = json
                .get("details")
                .and_then(Value::as_str)
                .unwrap_or("Unknown error");
            return Err(eyre::eyre!("Scryfall API error: {}", error_msg));
        }

        Ok(json.as_object().and_then(parsing::parse_card))
    }

    #[allow(unused_variables)]
    async fn bulk_search_cards(
        &self,
        cards: Vec<(SetCode, CollectorNumber)>,
    ) -> eyre::Result<Vec<(SetCode, CollectorNumber, CardID)>> {
        Ok(vec![])
    }

    async fn update_backend(&self) -> eyre::Result<bool> {
        // "Update" only means something when bulk mode is configured -- the
        // live API has no local state to refresh.
        if let Some(path) = &self.bulk_db_path {
            let new_conn = bulk::refresh_bulk_db(path).await?;
            *self.bulk_conn.lock().await = Some(new_conn);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn get_card_prices(&self, uuid: &str) -> eyre::Result<Option<CardPrices>> {
        Ok(self
            .get_bulk_card_prices(vec![uuid.to_string()])
            .await?
            .remove(uuid))
    }

    async fn get_bulk_card_prices(
        &self,
        uuids: Vec<String>,
    ) -> eyre::Result<HashMap<String, CardPrices>> {
        let mut result = HashMap::new();
        let mut remaining = uuids;

        // Bulk database first: Scryfall's card JSON carries a `prices` object
        // on every card, and we already store that raw JSON locally, so this
        // is a zero-network lookup for anything the local snapshot has.
        {
            let bulk = self.bulk_conn.lock().await;
            if let Some(conn) = bulk.as_ref() {
                let mut still_missing = Vec::new();
                for id in remaining {
                    match bulk::get_card(conn, &id) {
                        Ok(Some(raw)) => {
                            if let Some(prices) = Self::extract_prices(&id, &raw) {
                                result.insert(id, prices);
                            } else {
                                still_missing.push(id);
                            }
                        }
                        _ => still_missing.push(id),
                    }
                }
                remaining = still_missing;
            }
        }

        if remaining.is_empty() {
            return Ok(result);
        }

        // Anything not in the bulk snapshot (or bulk mode isn't configured)
        // falls through to the live API, batched the same way card lookups
        // are. Prices aren't cached the way card data is -- they're expected
        // to change day to day, so each call gets a reasonably fresh value
        // for whatever the bulk snapshot didn't cover.
        for chunk in remaining.chunks(75) {
            match Self::fetch_batch_raw(chunk).await {
                Ok(raw_cards) => {
                    for (id, raw) in &raw_cards {
                        if let Some(prices) = Self::extract_prices(id, raw) {
                            result.insert(id.clone(), prices);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Scryfall: failed to fetch a batch of {} card(s) for prices, skipping them: {e}",
                        chunk.len()
                    );
                }
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests;
