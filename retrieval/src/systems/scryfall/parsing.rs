use std::collections::HashMap;

use models::{Card, CardColour, CardIdentifiers, MagicCard};
use serde_json::{Map, Value};

pub fn parse_color_identity(arr: &[Value]) -> Vec<CardColour> {
    arr.iter()
        .filter_map(Value::as_str)
        .map(|c| match c {
            "B" => CardColour::Black,
            "U" => CardColour::Blue,
            "W" => CardColour::White,
            "G" => CardColour::Green,
            "R" => CardColour::Red,
            _ => CardColour::Colourless,
        })
        .collect()
}

pub fn parse_string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Scryfall's `legalities` object maps format -> `"legal" | "not_legal" | "banned" |
/// "restricted"`; normalise to mtgjson's `"Legal" | "Not Legal" | "Banned" | "Restricted"`
/// so callers see the same casing regardless of backend.
pub fn parse_legalities(value: Option<&Value>) -> HashMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(format, status)| {
                    let status = status.as_str()?;
                    let readable = status
                        .split('_')
                        .map(|word| {
                            let mut chars = word.chars();
                            match chars.next() {
                                Some(first) => {
                                    first.to_uppercase().collect::<String>() + chars.as_str()
                                }
                                None => String::new(),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    Some((format.clone(), readable))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Returns the first entry of `card_faces`, if present. Cards with multiple
/// faces (modal double-faced cards, transform cards, split cards, adventures)
/// omit several gameplay fields from the top-level card object -- Scryfall
/// nests them per-face instead. This is used as a fallback source for those
/// fields so multi-faced cards aren't dropped from results.
fn front_face(card: &Map<String, Value>) -> Option<&Map<String, Value>> {
    card.get("card_faces")
        .and_then(Value::as_array)
        .and_then(|faces| faces.first())
        .and_then(Value::as_object)
}

/// Reads a string field from the top-level card object, falling back to the
/// same key on the first card face if the top-level field is absent. This is
/// what multi-faced cards need for fields like `oracle_text` and `mana_cost`.
fn str_field_with_face_fallback<'a>(
    card: &'a Map<String, Value>,
    face: Option<&'a Map<String, Value>>,
    key: &str,
) -> Option<&'a str> {
    card.get(key)
        .and_then(Value::as_str)
        .or_else(|| face.and_then(|f| f.get(key)).and_then(Value::as_str))
}

/// Parses a single Scryfall card JSON object into a `Card::Magic`, shared by
/// both the search-results list mapping and the random-card lookup. Returns
/// `None` if any field required to construct a `MagicCard` is missing.
///
/// Cards with multiple faces (MDFCs like Birgi, God of Storytelling //
/// Harnfel, Horn of Bounty; transform cards; split cards; adventures) don't
/// carry `oracle_text`, `mana_cost`, `power`, `toughness`, `colors`, or
/// `flavor_text` on the top-level object -- Scryfall nests those under
/// `card_faces` instead. We fall back to the first face for these fields so
/// such cards are still returned instead of being silently dropped.
pub fn parse_card(card: &Map<String, Value>) -> Option<Card> {
    let card_name = card.get("name")?.as_str()?;
    let card_id = card.get("id")?.as_str()?;
    let set_code = card.get("set")?.as_str()?;
    let rarity = card.get("rarity")?.as_str()?;
    let collector_number = card.get("collector_number")?.as_str()?;

    let face = front_face(card);

    let artist = str_field_with_face_fallback(card, face, "artist").unwrap_or_default();
    let oracle_text = str_field_with_face_fallback(card, face, "oracle_text").unwrap_or_default();

    let color_identity = card
        .get("color_identity")
        .and_then(Value::as_array)
        .map(|arr| parse_color_identity(arr))
        .unwrap_or_default();

    let type_line = card.get("type_line")?.as_str()?;
    let mut types = vec![];
    let mut subtypes = vec![];
    let mut supertypes = vec![];

    let parts: Vec<&str> = type_line.split("—").map(|p| p.trim()).collect();
    if !parts.is_empty() {
        let type_part = parts[0];
        let type_tokens: Vec<&str> = type_part.split(' ').collect();
        for token in type_tokens {
            match token {
                // TODO: add the rest
                "Legendary" | "Basic" | "World" => supertypes.push(token.to_string()),
                _ => types.push(token.to_string()),
            }
        }
    }
    if parts.len() > 1 {
        let subtype_part = parts[1];
        let subtype_tokens: Vec<&str> = subtype_part.split(' ').collect();
        subtypes = subtype_tokens.iter().map(|s| s.to_string()).collect();
    }

    let colors_value = card
        .get("colors")
        .and_then(Value::as_array)
        .or_else(|| face.and_then(|f| f.get("colors")).and_then(Value::as_array));

    Some(Card::Magic(MagicCard {
        name: card_name.to_string(),
        set_code: set_code.to_string(),
        artist: artist.to_string(),
        color_identity,
        id: card_id.to_string(),
        rarity: rarity.to_string().into(),
        text: oracle_text.to_string(),
        card_identifiers: CardIdentifiers {
            scryfall_id: card_id.to_string(),
            id: card_id.to_string(),
        },
        collector_number: collector_number.to_string(),
        subtypes,
        supertypes,
        types,
        mana_cost: str_field_with_face_fallback(card, face, "mana_cost")
            .unwrap_or_default()
            .to_string(),
        mana_value: card.get("cmc").and_then(Value::as_f64).unwrap_or_default(),
        type_line: type_line.to_string(),
        power: str_field_with_face_fallback(card, face, "power").map(String::from),
        toughness: str_field_with_face_fallback(card, face, "toughness").map(String::from),
        loyalty: str_field_with_face_fallback(card, face, "loyalty").map(String::from),
        defense: str_field_with_face_fallback(card, face, "defense").map(String::from),
        keywords: parse_string_array(card.get("keywords")),
        colors: colors_value.map(|arr| parse_color_identity(arr)).unwrap_or_default(),
        legalities: parse_legalities(card.get("legalities")),
        finishes: parse_string_array(card.get("finishes")),
        is_reserved: card
            .get("reserved")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_promo: card.get("promo").and_then(Value::as_bool).unwrap_or(false),
        is_reprint: card
            .get("reprint")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        border_color: card
            .get("border_color")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        frame_effects: parse_string_array(card.get("frame_effects")),
        is_full_art: card
            .get("full_art")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        watermark: card
            .get("watermark")
            .and_then(Value::as_str)
            .map(String::from),
        flavor_text: str_field_with_face_fallback(card, face, "flavor_text").map(String::from),
        set_name: card
            .get("set_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }))
}
