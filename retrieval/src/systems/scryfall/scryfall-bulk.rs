//! Downloads and locally queries Scryfall's "default_cards" bulk data file
//! (https://scryfall.com/docs/api/bulk-data), instead of hitting the live
//! Scryfall API for every card lookup. This is what lets collection search
//! (and everything else routed through `ScryfallRetrievalSystem`) run
//! without touching the network at all once a local snapshot exists.
//!
//! The snapshot is a plain SQLite database we build ourselves: one row per
//! card, storing the *entire* Scryfall card JSON object as a blob alongside
//! a few indexed columns for the lookups that matter (name, set + collector
//! number). Storing the full object rather than remapping every field into
//! columns means card parsing (`parsing::parse_card`, with its multi-faced
//! card handling) is reused unchanged for both the live API and this local
//! database -- one parser, one set of edge cases handled.

use std::{
    fs,
    io::BufReader,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use crate::http::stream_to_file;

/// How old the local snapshot can get before a search triggers a fresh
/// download. Scryfall regenerates `default_cards` roughly once a day, so
/// this matches that cadence -- refreshing more often wouldn't find new data
/// anyway.
const REFRESH_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// How often the background task wakes up to check whether a refresh is
/// due. Cheap to check hourly; the actual multi-hundred-MB download only
/// happens when `REFRESH_INTERVAL_SECS` has actually elapsed.
pub const CHECK_INTERVAL_SECS: u64 = 60 * 60;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Opens (creating if necessary) the local bulk card database and ensures
/// its schema exists. Safe to call on a brand new, empty path.
pub fn open_bulk_db(path: &str) -> eyre::Result<Connection> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS cards (
            id TEXT PRIMARY KEY,
            name_lower TEXT NOT NULL,
            set_code TEXT NOT NULL,
            collector_number TEXT NOT NULL,
            raw_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_bulk_cards_name_lower ON cards(name_lower);
        CREATE INDEX IF NOT EXISTS idx_bulk_cards_set_cn ON cards(set_code, collector_number);
        CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    Ok(conn)
}

/// Unix timestamp (seconds) this database was last successfully refreshed,
/// or `None` if it's an empty/never-populated database.
pub fn last_updated(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'last_updated'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| s.parse::<i64>().ok())
}

fn is_stale(conn: &Connection) -> bool {
    match last_updated(conn) {
        Some(ts) => now_secs() - ts > REFRESH_INTERVAL_SECS,
        None => true,
    }
}

/// Downloads Scryfall's current `default_cards` bulk file and rebuilds the
/// local database from scratch, then atomically swaps it into place at
/// `path` (via a temp file + rename, so a query running concurrently never
/// sees a half-written database). Returns a fresh connection to the
/// rebuilt database on success.
pub async fn refresh_bulk_db(path: &str) -> eyre::Result<Connection> {
    tracing::info!("Scryfall bulk: fetching the current bulk-data file listing");
    let listing: Value = reqwest::Client::new()
        .get("https://api.scryfall.com/bulk-data")
        .header(reqwest::header::USER_AGENT, "gathers_cli/1.0")
        .header(reqwest::header::ACCEPT, "*/*")
        .send()
        .await?
        .json()
        .await?;

    let download_uri = listing
        .get("data")
        .and_then(Value::as_array)
        .and_then(|entries| {
            entries
                .iter()
                .find(|e| e.get("type").and_then(Value::as_str) == Some("default_cards"))
        })
        .and_then(|entry| entry.get("download_uri"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            eyre::eyre!("Scryfall bulk-data listing did not include a default_cards entry")
        })?
        .to_string();

    let tmp_json_path = PathBuf::from(format!("{path}.download.json"));
    tracing::info!(
        "Scryfall bulk: downloading default_cards ({}), this can take a few minutes",
        download_uri
    );
    stream_to_file(
        &download_uri,
        "Scryfall default_cards",
        &tmp_json_path,
        None,
        "Downloading Scryfall bulk card data",
    )
    .await?;

    tracing::info!("Scryfall bulk: parsing downloaded card data");
    let file = fs::File::open(&tmp_json_path)?;
    let cards: Vec<Value> = serde_json::from_reader(BufReader::new(file))?;

    let tmp_db_path = format!("{path}.new");
    // Start clean in case a previous run was interrupted and left a partial
    // file behind.
    let _ = fs::remove_file(&tmp_db_path);

    {
        let mut conn = open_bulk_db(&tmp_db_path)?;
        tracing::info!(
            "Scryfall bulk: importing {} cards into local database",
            cards.len()
        );
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO cards (id, name_lower, set_code, collector_number, raw_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for card in &cards {
                let Some(id) = card.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(name) = card.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let set_code = card.get("set").and_then(Value::as_str).unwrap_or_default();
                let collector_number = card
                    .get("collector_number")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let raw = serde_json::to_string(card)?;
                stmt.execute(rusqlite::params![
                    id,
                    name.to_lowercase(),
                    set_code,
                    collector_number,
                    raw
                ])?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('last_updated', ?1)",
            rusqlite::params![now_secs().to_string()],
        )?;
        tx.commit()?;
        // `conn` drops here, closing the file handle -- required before the
        // rename below on platforms that don't allow renaming open files.
    }

    fs::rename(&tmp_db_path, path)?;
    let _ = fs::remove_file(&tmp_json_path);

    tracing::info!(
        "Scryfall bulk: refresh complete, {} cards loaded",
        cards.len()
    );
    open_bulk_db(path)
}

/// Ensures the bulk database at `path` exists and is fresh, downloading and
/// rebuilding it if it's missing or older than `REFRESH_INTERVAL_SECS`.
/// Returns `Some(conn)` if a refresh actually happened (so the caller can
/// swap the new connection in), or `None` if the existing database was
/// already fresh and nothing needed to change.
pub async fn ensure_fresh(path: &str) -> eyre::Result<Option<Connection>> {
    let needs_refresh = if Path::new(path).exists() {
        match open_bulk_db(path) {
            Ok(conn) => is_stale(&conn),
            Err(_) => true,
        }
    } else {
        true
    };

    if needs_refresh {
        Ok(Some(refresh_bulk_db(path).await?))
    } else {
        Ok(None)
    }
}

/// Looks up a single card by Scryfall ID in the local database.
pub fn get_card(conn: &Connection, id: &str) -> eyre::Result<Option<Value>> {    let raw: Option<String> = conn
        .query_row("SELECT raw_json FROM cards WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(match raw {
        Some(raw) => Some(serde_json::from_str(&raw)?),
        None => None,
    })
}

/// Returns a single uniformly-random card's raw JSON, or `None` if the
/// database is empty.
pub fn get_random_card(conn: &Connection) -> eyre::Result<Option<Value>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT raw_json FROM cards ORDER BY RANDOM() LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match raw {
        Some(raw) => Some(serde_json::from_str(&raw)?),
        None => None,
    })
}

/// Searches the local database by name (substring, or exact when
/// `exact_name` is set -- used for "every printing of this card") and
/// optional set code / collector number, returning raw card JSON rows.
pub fn search(
    conn: &Connection,
    name: Option<&str>,
    exact_name: bool,
    set_code: Option<&str>,
    collector_number: Option<&str>,
    limit: i64,
    offset: i64,
) -> eyre::Result<Vec<Value>> {
    let mut conditions: Vec<String> = Vec::new();
    let mut bind_params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(name) = name
        && !name.is_empty()
    {
        if exact_name {
            conditions.push("name_lower = ?".to_string());
            bind_params.push(Box::new(name.to_lowercase()));
        } else {
            conditions.push("name_lower LIKE ?".to_string());
            bind_params.push(Box::new(format!("%{}%", name.to_lowercase())));
        }
    }
    if let Some(set_code) = set_code
        && !set_code.is_empty()
    {
        conditions.push("set_code = ?".to_string());
        bind_params.push(Box::new(set_code.to_lowercase()));
    }
    if let Some(cn) = collector_number
        && !cn.is_empty()
    {
        conditions.push("collector_number = ?".to_string());
        bind_params.push(Box::new(cn.to_string()));
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    bind_params.push(Box::new(limit));
    bind_params.push(Box::new(offset));
    let param_refs: Vec<&dyn rusqlite::ToSql> = bind_params.iter().map(|b| b.as_ref()).collect();

    // A plain name search legitimately matches every printing of every card
    // whose name contains the search text (that's the whole point of the
    // `cards` table having one row per printing). Scryfall's live API
    // collapses this down to one representative printing per card name by
    // default (`unique=cards`) -- without doing the same here, a search for
    // e.g. "Avatar Aang" would return every printing/treatment of that card
    // (regular, surge foil, showcase, ...) as separate results instead of
    // one. `exact_name` (used for the "choose printing" flow) deliberately
    // wants every printing, so it skips this collapsing.
    let sql = if exact_name {
        format!(
            "SELECT raw_json FROM cards {where_clause} \
             ORDER BY set_code ASC, collector_number ASC LIMIT ? OFFSET ?"
        )
    } else {
        format!(
            "SELECT raw_json FROM (
                 SELECT raw_json, name_lower,
                        ROW_NUMBER() OVER (
                            PARTITION BY name_lower
                            ORDER BY set_code DESC, collector_number DESC
                        ) AS rn
                 FROM cards {where_clause}
             )
             WHERE rn = 1
             ORDER BY name_lower ASC
             LIMIT ? OFFSET ?"
        )
    };

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for raw in rows {
        out.push(serde_json::from_str(&raw?)?);
    }
    Ok(out)
}
