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
    open_local(path)
}

/// Open an existing collection SQLite file.
pub fn open_local(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.create_collation("unicase", |a, b| {
        unicase::UniCase::new(a).cmp(&unicase::UniCase::new(b))
    })?;
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

fn has_table(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .map(|c| c > 0)
    .unwrap_or(false)
}

// ── Decks ──────────────────────────────────────────────────────────

/// List all decks in the collection. Returns Vec<(id, name)>.
pub fn list_decks(conn: &Connection) -> Result<Vec<(i64, String)>> {
    if has_table(conn, "decks") {
        let mut stmt = conn.prepare("SELECT id, name FROM decks")?;
        let decks = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        return Ok(decks);
    }

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
    if has_table(conn, "decks") {
        let existing: Option<i64> = conn
            .query_row("SELECT id FROM decks WHERE name = ?1", [name], |r| r.get(0))
            .ok();
        if let Some(id) = existing {
            return Ok(id);
        }
        let id = now_epoch_ms();
        conn.execute(
            "INSERT INTO decks (id, name, mtime_secs, usn, common, kind) VALUES (?1, ?2, ?3, -1, x'', x'')",
            rusqlite::params![id, name, now_epoch_secs()],
        )?;
        Ok(id)
    } else {
        let decks_json: String = conn.query_row("SELECT decks FROM col", [], |r| r.get(0))?;
        let mut decks: HashMap<String, Value> = serde_json::from_str(&decks_json)?;

        for (id_str, deck) in &decks {
            if deck["name"].as_str() == Some(name) {
                return Ok(id_str.parse().unwrap_or(0));
            }
        }

        let id = now_epoch_ms();
        let deck = serde_json::json!({
            "id": id, "name": name, "mod": now_epoch_secs(), "usn": -1,
            "lrnToday": [0, 0], "revToday": [0, 0], "newToday": [0, 0],
            "timeToday": [0, 0], "collapsed": false, "browserCollapsed": false,
            "desc": "", "dyn": 0, "conf": 1, "extendNew": 0, "extendRev": 0,
        });
        decks.insert(id.to_string(), deck);
        conn.execute("UPDATE col SET decks = ?1", [&serde_json::to_string(&decks)?])?;
        Ok(id)
    }
}

// ── Notetypes / Models ─────────────────────────────────────────────

/// Extract ordered field names from a notetype.
/// New schema: fields stored in protobuf `config` blob — but field names
/// are also stored in a `fields` table.
/// Legacy schema: JSON `flds` array in the models JSON.
fn get_model_fields_new(conn: &Connection, model_id: i64) -> Result<Vec<String>> {
    // New schema has a `fields` table
    if has_table(conn, "fields") {
        let mut stmt =
            conn.prepare("SELECT name FROM fields WHERE ntid = ?1 ORDER BY ord")?;
        let fields: Vec<String> = stmt
            .query_map([model_id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if !fields.is_empty() {
            return Ok(fields);
        }
    }
    Err(anyhow!("no fields found for notetype {model_id}"))
}

fn get_model_fields_legacy(model: &Value) -> Vec<String> {
    model["flds"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|f| f["name"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// List all notetypes with their fields. Returns Vec<(id, name, fields)>.
pub fn list_notetypes(conn: &Connection) -> Result<Vec<(i64, String, Vec<String>)>> {
    if has_table(conn, "notetypes") {
        let mut stmt = conn.prepare("SELECT id, name FROM notetypes ORDER BY name")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut result = Vec::new();
        for (id, name) in rows {
            let fields = get_model_fields_new(conn, id).unwrap_or_default();
            result.push((id, name, fields));
        }
        return Ok(result);
    }

    // Legacy
    let models_json: String = conn.query_row("SELECT models FROM col", [], |r| r.get(0))?;
    let models: HashMap<String, Value> = serde_json::from_str(&models_json)?;
    let mut result = Vec::new();
    for (id_str, model) in &models {
        let id: i64 = id_str.parse().unwrap_or(0);
        let name = model["name"].as_str().unwrap_or("").to_string();
        let fields = get_model_fields_legacy(model);
        result.push((id, name, fields));
    }
    result.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(result)
}

/// Find a model by name, returning (id, field_names).
pub fn find_model_by_name(conn: &Connection, name: &str) -> Result<(i64, Vec<String>)> {
    if has_table(conn, "notetypes") {
        let id: i64 = conn
            .query_row("SELECT id FROM notetypes WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .context(format!("notetype '{}' not found", name))?;
        let fields = get_model_fields_new(conn, id)?;
        return Ok((id, fields));
    }

    // Legacy
    let models_json: String = conn.query_row("SELECT models FROM col", [], |r| r.get(0))?;
    let models: HashMap<String, Value> = serde_json::from_str(&models_json)?;
    for (id_str, model) in &models {
        if model["name"].as_str() == Some(name) {
            let id: i64 = id_str.parse().unwrap_or(0);
            let fields = get_model_fields_legacy(model);
            return Ok((id, fields));
        }
    }
    Err(anyhow!("notetype '{}' not found", name))
}

// ── Note/Card creation ─────────────────────────────────────────────

fn next_due_position(conn: &Connection, deck_id: i64) -> i64 {
    conn.query_row(
        "SELECT COALESCE(MAX(due), 0) + 1 FROM cards WHERE did = ?1 AND type = 0",
        [deck_id],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Count how many card templates a notetype has (for generating multiple cards).
fn card_template_count(conn: &Connection, model_id: i64) -> i64 {
    if has_table(conn, "templates") {
        conn.query_row(
            "SELECT count(*) FROM templates WHERE ntid = ?1",
            [model_id],
            |r| r.get(0),
        )
        .unwrap_or(1)
    } else {
        // Legacy: count from models JSON
        // For simplicity, default to 1
        1
    }
}

/// Add a note with arbitrary field values.
/// `field_values` must be in the same order as the notetype's fields.
/// `due_in_secs`: if Some, schedule as a review card due in this many seconds.
pub fn add_note_with_fields(
    conn: &Connection,
    deck_id: i64,
    model_id: i64,
    field_values: &[String],
    tags: &str,
    due_in_secs: Option<i64>,
) -> Result<i64> {
    let note_id = now_epoch_ms();
    let guid = random_guid();
    let flds = field_values.join("\x1f");
    let sfld = field_values.first().map(|s| s.as_str()).unwrap_or("");
    let csum = field_checksum(sfld);
    let mod_time = now_epoch_secs();

    conn.execute(
        "INSERT INTO notes (id, guid, mid, mod, usn, tags, flds, sfld, csum, flags, data) \
         VALUES (?1, ?2, ?3, ?4, -1, ?5, ?6, ?7, ?8, 0, '')",
        rusqlite::params![note_id, guid, model_id, mod_time, tags, flds, sfld, csum],
    )?;

    // Create one card per template
    let num_templates = card_template_count(conn, model_id);
    for ord in 0..num_templates {
        let card_id = note_id + 1 + ord;

        // type 0 = new, queue 0 = new, due = position
        // type 2 = review, queue 2 = review, due = epoch day
        let (card_type, queue, due, ivl, factor) = if let Some(secs) = due_in_secs {
            let due_days = secs / 86400;
            // Anki review card `due` = days since collection creation (col.crt)
            let crt: i64 = conn.query_row("SELECT crt FROM col", [], |r| r.get(0))
                .unwrap_or(mod_time);
            let today_day = (mod_time - crt) / 86400;
            let due_day = today_day + due_days.max(1);
            // Review card: type=2, queue=2, due=days since crt, ivl=days, factor=2500 (default ease)
            (2i64, 2i64, due_day, due_days.max(1), 2500i64)
        } else {
            let pos = next_due_position(conn, deck_id);
            (0, 0, pos, 0, 0)
        };

        conn.execute(
            "INSERT INTO cards (id, nid, did, ord, mod, usn, type, queue, due, ivl, factor, reps, lapses, left, odue, odid, flags, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, -1, ?6, ?7, ?8, ?9, ?10, 0, 0, 0, 0, 0, 0, '')",
            rusqlite::params![card_id, note_id, deck_id, ord, mod_time, card_type, queue, due, ivl, factor],
        )?;
    }

    tracing::info!(%note_id, %deck_id, templates = num_templates, "added note and card(s)");
    Ok(note_id)
}

/// Reschedule cards matching a query or note ID.
/// Sets them as review cards with the given due-in offset.
/// Returns the number of cards updated.
pub fn reschedule_cards(
    conn: &Connection,
    query: Option<&str>,
    note_id: Option<i64>,
    due_in_secs: i64,
) -> Result<usize> {
    let now = now_epoch_secs();
    let crt: i64 = conn.query_row("SELECT crt FROM col", [], |r| r.get(0))
        .unwrap_or(now);
    let today_day = (now - crt) / 86400;
    let due_days = (due_in_secs / 86400).max(1);
    let due_day = today_day + due_days;

    let card_ids: Vec<i64> = if let Some(nid) = note_id {
        let mut stmt = conn.prepare("SELECT id FROM cards WHERE nid = ?1")?;
        let ids: Vec<i64> = stmt.query_map([nid], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
        ids
    } else if let Some(q) = query {
        let pattern = format!("%{q}%");
        let mut stmt = conn.prepare(
            "SELECT c.id FROM cards c JOIN notes n ON c.nid = n.id WHERE n.sfld LIKE ?1"
        )?;
        let ids: Vec<i64> = stmt.query_map([&pattern], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
        ids
    } else {
        anyhow::bail!("must specify either --query or --note-id");
    };

    if card_ids.is_empty() {
        anyhow::bail!("no cards found matching the criteria");
    }

    for cid in &card_ids {
        conn.execute(
            "UPDATE cards SET type = 2, queue = 2, due = ?1, ivl = ?2, factor = 2500, mod = ?3, usn = -1 WHERE id = ?4",
            rusqlite::params![due_day, due_days, now, cid],
        )?;
    }

    tracing::info!(count = card_ids.len(), due_day, "rescheduled cards");
    Ok(card_ids.len())
}

/// A search result entry.
pub struct SearchResult {
    pub note_id: i64,
    pub deck_name: String,
    pub card_type: String,
    pub due_display: String,
    pub fields_preview: String,
}

/// Search for cards/notes by text across all fields.
pub fn search_cards(
    conn: &Connection,
    query: &str,
    deck_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let pattern = format!("%{query}%");

    // Build deck name lookup
    let decks = list_decks(conn)?;
    let deck_map: std::collections::HashMap<i64, String> = decks.into_iter().collect();

    // Filter deck ID if specified
    let deck_id_filter: Option<i64> = deck_filter.and_then(|name| {
        deck_map.iter().find(|(_, n)| n.as_str() == name).map(|(id, _)| *id)
    });

    let sql_with_deck =
        "SELECT c.id, c.nid, c.did, c.type, c.queue, c.due, c.ivl, n.flds, n.sfld \
         FROM cards c JOIN notes n ON c.nid = n.id \
         WHERE c.did = ?2 AND n.flds LIKE ?1 \
         ORDER BY n.mod DESC LIMIT ?3";
    let sql_no_deck =
        "SELECT c.id, c.nid, c.did, c.type, c.queue, c.due, c.ivl, n.flds, n.sfld \
         FROM cards c JOIN notes n ON c.nid = n.id \
         WHERE n.flds LIKE ?1 \
         ORDER BY n.mod DESC LIMIT ?2";

    let crt: i64 = conn.query_row("SELECT crt FROM col", [], |r| r.get(0)).unwrap_or(0);

    let rows: Vec<SearchResult> = if let Some(did) = deck_id_filter {
        let mut stmt = conn.prepare(sql_with_deck)?;
        let mapped = stmt.query_map(rusqlite::params![pattern, did, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(1)?,  // nid
                r.get::<_, i64>(2)?,  // did
                r.get::<_, i64>(3)?,  // type
                r.get::<_, i64>(5)?,  // due
                r.get::<_, String>(7)?,  // flds
            ))
        })?
        .filter_map(|r| r.ok())
        .map(|(nid, did, ctype, due, flds)| to_search_result(nid, did, ctype, due, &flds, &deck_map, crt))
        .collect();
        mapped
    } else {
        let mut stmt = conn.prepare(sql_no_deck)?;
        let mapped = stmt.query_map(rusqlite::params![pattern, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(7)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .map(|(nid, did, ctype, due, flds)| to_search_result(nid, did, ctype, due, &flds, &deck_map, crt))
        .collect();
        mapped
    };

    Ok(rows)
}

fn to_search_result(
    nid: i64,
    did: i64,
    ctype: i64,
    due: i64,
    flds: &str,
    deck_map: &std::collections::HashMap<i64, String>,
    crt: i64,
) -> SearchResult {
    let deck_name = deck_map.get(&did).cloned().unwrap_or_else(|| did.to_string());
    let card_type = match ctype {
        0 => "new".to_string(),
        1 => "learning".to_string(),
        2 => "review".to_string(),
        3 => "relearning".to_string(),
        _ => format!("type:{ctype}"),
    };
    let due_display = if ctype == 0 {
        format!("pos:{due}")
    } else if ctype == 2 {
        let due_epoch = crt + due * 86400;
        let dt = chrono::DateTime::from_timestamp(due_epoch, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| format!("day:{due}"));
        dt
    } else {
        format!("{due}")
    };
    let fields: Vec<&str> = flds.split('\x1f').collect();
    let preview = fields.iter().take(3).cloned().collect::<Vec<_>>().join(" | ");
    let fields_preview = if preview.len() > 80 {
        format!("{}…", &preview[..77])
    } else {
        preview
    };
    SearchResult { note_id: nid, deck_name, card_type, due_display, fields_preview }
}
