//! Anki collection SQLite manipulation.

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Open a collection from raw bytes (writes to a temp file, returns connection + path).
pub fn open_collection(data: &[u8], path: &std::path::Path) -> Result<Connection> {
    std::fs::write(path, data)?;
    let conn = Connection::open(path)?;
    Ok(conn)
}

/// Read the collection back to bytes.
pub fn read_collection(path: &std::path::Path) -> Result<Vec<u8>> {
    std::fs::read(path).context("reading collection file")
}

fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn field_checksum(field: &str) -> i64 {
    let stripped = field.trim();
    let mut hasher = Sha1::new();
    hasher.update(stripped.as_bytes());
    let hash = hasher.finalize();
    let hex = format!("{:x}", hash);
    // First 8 hex digits as u32
    let val = u32::from_str_radix(&hex[..8], 16).unwrap_or(0);
    val as i64
}

fn random_guid() -> String {
    use rand::Rng;
    let table = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!#$%&()*+,-./:;<=>?@[]^_`{|}~";
    let mut rng = rand::thread_rng();
    (0..10)
        .map(|_| table[rng.gen_range(0..table.len())] as char)
        .collect()
}

/// List all decks in the collection. Returns Vec<(id, name)>.
pub fn list_decks(conn: &Connection) -> Result<Vec<(i64, String)>> {
    // Try new schema first (Anki 2.1.28+): separate `decks` table
    let has_decks_table: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='decks'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if has_decks_table {
        let mut stmt = conn.prepare("SELECT id, name FROM decks")?;
        let decks = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        return Ok(decks);
    }

    // Legacy schema: decks JSON in col table
    let decks_json: String = conn.query_row("SELECT decks FROM col", [], |r| r.get(0))?;
    let decks: HashMap<String, Value> = serde_json::from_str(&decks_json)?;
    let mut result = Vec::new();
    for (id_str, deck) in &decks {
        let id: i64 = id_str.parse().unwrap_or(0);
        let name = deck["name"].as_str().unwrap_or("").to_string();
        result.push((id, name));
    }
    result.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(result)
}

/// Find or create a deck by name. Returns deck id.
pub fn find_or_create_deck(conn: &Connection, name: &str) -> Result<i64> {
    let has_decks_table: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='decks'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if has_decks_table {
        // New schema
        let existing: Option<i64> = conn
            .query_row("SELECT id FROM decks WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .ok();
        if let Some(id) = existing {
            return Ok(id);
        }
        let id = now_epoch_ms();
        // Minimal deck blob — Anki stores protobuf in `common` and `kind` columns in newer versions
        // but also accepts a simple JSON-like setup. Let's insert with empty blobs.
        conn.execute(
            "INSERT INTO decks (id, name, mtime_secs, usn, common, kind) VALUES (?1, ?2, ?3, -1, x'', x'')",
            rusqlite::params![id, name, now_epoch_secs()],
        )?;
        Ok(id)
    } else {
        // Legacy schema
        let decks_json: String = conn.query_row("SELECT decks FROM col", [], |r| r.get(0))?;
        let mut decks: HashMap<String, Value> = serde_json::from_str(&decks_json)?;

        for (id_str, deck) in &decks {
            if deck["name"].as_str() == Some(name) {
                return Ok(id_str.parse().unwrap_or(0));
            }
        }

        let id = now_epoch_ms();
        let deck = serde_json::json!({
            "id": id,
            "name": name,
            "mod": now_epoch_secs(),
            "usn": -1,
            "lrnToday": [0, 0],
            "revToday": [0, 0],
            "newToday": [0, 0],
            "timeToday": [0, 0],
            "collapsed": false,
            "browserCollapsed": false,
            "desc": "",
            "dyn": 0,
            "conf": 1,
            "extendNew": 0,
            "extendRev": 0,
        });
        decks.insert(id.to_string(), deck);
        let new_json = serde_json::to_string(&decks)?;
        conn.execute("UPDATE col SET decks = ?1", [&new_json])?;
        Ok(id)
    }
}

/// Find the "Basic" model/notetype id, or the first available one.
pub fn find_basic_model(conn: &Connection) -> Result<i64> {
    let has_notetypes_table: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='notetypes'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if has_notetypes_table {
        // Try to find "Basic"
        let basic: Option<i64> = conn
            .query_row(
                "SELECT id FROM notetypes WHERE name = 'Basic'",
                [],
                |r| r.get(0),
            )
            .ok();
        if let Some(id) = basic {
            return Ok(id);
        }
        // Fall back to first notetype
        conn.query_row("SELECT id FROM notetypes ORDER BY id LIMIT 1", [], |r| {
            r.get(0)
        })
        .context("no notetypes found")
    } else {
        // Legacy: models JSON in col table
        let models_json: String = conn.query_row("SELECT models FROM col", [], |r| r.get(0))?;
        let models: HashMap<String, Value> = serde_json::from_str(&models_json)?;

        for (id_str, model) in &models {
            if model["name"].as_str() == Some("Basic") {
                return Ok(id_str.parse().unwrap_or(0));
            }
        }
        // Fall back to first
        models
            .keys()
            .next()
            .and_then(|k| k.parse().ok())
            .ok_or_else(|| anyhow!("no models found in collection"))
    }
}

/// Get the next card `due` position for new cards in a deck.
fn next_due_position(conn: &Connection, deck_id: i64) -> i64 {
    conn.query_row(
        "SELECT COALESCE(MAX(due), 0) + 1 FROM cards WHERE did = ?1 AND type = 0",
        [deck_id],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Add a note with Front/Back fields to a deck.
pub fn add_note(
    conn: &Connection,
    deck_id: i64,
    model_id: i64,
    front: &str,
    back: &str,
    tags: &str,
) -> Result<i64> {
    let note_id = now_epoch_ms();
    let guid = random_guid();
    let flds = format!("{}\x1f{}", front, back);
    let sfld = front;
    let csum = field_checksum(sfld);
    let mod_time = now_epoch_secs();

    conn.execute(
        "INSERT INTO notes (id, guid, mid, mod, usn, tags, flds, sfld, csum, flags, data) \
         VALUES (?1, ?2, ?3, ?4, -1, ?5, ?6, ?7, ?8, 0, '')",
        rusqlite::params![note_id, guid, model_id, mod_time, tags, flds, sfld, csum],
    )?;

    let card_id = note_id + 1;
    let due = next_due_position(conn, deck_id);

    conn.execute(
        "INSERT INTO cards (id, nid, did, ord, mod, usn, type, queue, due, ivl, factor, reps, lapses, left, odue, odid, flags, data) \
         VALUES (?1, ?2, ?3, 0, ?4, -1, 0, 0, ?5, 0, 0, 0, 0, 0, 0, 0, 0, '')",
        rusqlite::params![card_id, note_id, deck_id, mod_time, due],
    )?;

    tracing::info!(%note_id, %card_id, %deck_id, "added note and card");
    Ok(note_id)
}
