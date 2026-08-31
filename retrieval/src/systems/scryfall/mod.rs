mod parsing;

use std::collections::HashMap;

use eyre::OptionExt;
use models::{
    Card, CardID, CardIdentifiers, CollectorNumber, SetCode,
    filters::{CardSearchFilters, SortField, SortOrder},
};
use serde_json::Value;

use crate::{NamedRetrievalSystem, RetrievalSystemTrait};

#[derive(Debug, Clone)]
pub struct ScryfallRetrievalSystem {}

impl NamedRetrievalSystem for ScryfallRetrievalSystem {
    fn name(&self) -> &str {
        "Scryfall"
    }
}

impl ScryfallRetrievalSystem {
    pub fn new() -> eyre::Result<Self> {
        Ok(Self {})
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
    async fn fetch_cards_batch(ids: &[String]) -> eyre::Result<HashMap<String, models::Card>> {
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
            if let Some(card) = parsing::parse_card(obj) {
                result.insert(id.to_string(), card);
            } else {
                tracing::warn!(
                    "Scryfall: failed to parse card {id} from a batch response, skipping it"
                );
            }
        }

        Ok(result)
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

        // Scryfall's bulk "collection" endpoint accepts up to 75 identifiers
        // per request. Fetching one card per request (the old approach) sends
        // as many requests as there are cards in the collection, which blows
        // through Scryfall's 10 req/sec limit for anything but small
        // collections. Chunking into batches of 75 keeps even a
        // several-hundred-card collection down to a handful of requests.
        let chunks: Vec<&[String]> = ids.chunks(75).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            if i > 0 {
                // A small pause between batches. Comfortably under the 10
                // req/sec limit even if other requests are happening
                // concurrently (e.g. a search running at the same time).
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }

            // A whole batch failing (network blip, transient 5xx, ...) must
            // not take down every other batch -- skip it and keep going so
            // one bad batch doesn't wipe out the rest of the collection.
            match Self::fetch_cards_batch(chunk).await {
                Ok(batch) => result.extend(batch),
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
        Ok(false)
    }
}

#[cfg(test)]
mod tests;
