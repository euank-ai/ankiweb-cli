//! Normal (incremental) sync protocol implementation.
//!
//! Instead of downloading the entire collection, modifying it, and uploading
//! (which clobbers any changes synced by other clients in between), this uses
//! Anki's normal sync protocol to push only our changes incrementally.
//!
//! Flow:
//! 1. Ensure a local collection exists (full download on first use)
//! 2. Make local modifications (add note with usn=-1)
//! 3. Normal sync: meta → start → applyGraves → applyChanges → chunk/applyChunk → sanityCheck2 → finish

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::sync::{self, SyncConfig};

// ── Wire format types ──────────────────────────────────────────────

#[derive(Serialize)]
struct StartRequest {
    #[serde(rename = "minUsn")]
    client_usn: i64,
    #[serde(rename = "lnewer")]
    local_is_newer: bool,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
struct Graves {
    #[serde(default)]
    cards: Vec<i64>,
    #[serde(default)]
    decks: Vec<i64>,
    #[serde(default)]
    notes: Vec<i64>,
}

#[derive(Serialize)]
struct ApplyGravesRequest {
    chunk: Graves,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct UnchunkedChanges {
    #[serde(default)]
    models: Vec<Value>,
    #[serde(default)]
    decks: (Vec<Value>, Vec<Value>), // (decks, deck_config)
    #[serde(default)]
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conf: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    crt: Option<i64>,
}

#[derive(Serialize)]
struct ApplyChangesRequest {
    changes: UnchunkedChanges,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct Chunk {
    #[serde(default)]
    done: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    revlog: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cards: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    notes: Vec<Value>,
}

#[derive(Serialize)]
struct ApplyChunkRequest {
    chunk: Chunk,
}

/// Sanity check counts: [[new, learn, review], cards, notes, revlog, graves, notetypes, decks, deck_config]
#[derive(Serialize)]
struct SanityCheckRequest {
    client: SanityCheckCounts,
}

#[derive(Serialize)]
struct SanityCheckCounts(
    (u32, u32, u32), // due counts
    u32,             // cards
    u32,             // notes
    u32,             // revlog
    u32,             // graves
    u32,             // notetypes
    u32,             // decks
    u32,             // deck_config
);

#[derive(Deserialize, Debug)]
struct SanityCheckResponse {
    status: String,
}

// ── Local collection meta ──────────────────────────────────────────

struct LocalMeta {
    modification: i64, // ms epoch
    schema: i64,       // ms epoch
    usn: i64,
    last_sync: i64, // ms epoch
}

fn read_local_meta(conn: &Connection) -> Result<LocalMeta> {
    conn.query_row("SELECT mod, scm, usn, ls FROM col", [], |row| {
        Ok(LocalMeta {
            modification: row.get(0)?,
            schema: row.get(1)?,
            usn: row.get(2)?,
            last_sync: row.get(3)?,
        })
    })
    .context("reading local meta from col table")
}

fn update_local_meta_after_sync(conn: &Connection, new_mod: i64, new_usn: i64) -> Result<()> {
    conn.execute(
        "UPDATE col SET ls = ?1, mod = ?1, usn = ?2",
        rusqlite::params![new_mod, new_usn],
    )?;
    Ok(())
}

fn mark_downloaded(conn: &Connection) -> Result<()> {
    conn.execute_batch("UPDATE col SET ls = mod")?;
    Ok(())
}

// ── Applying received data ─────────────────────────────────────────

fn apply_graves(conn: &Connection, graves: &Graves) -> Result<()> {
    for nid in &graves.notes {
        conn.execute("DELETE FROM notes WHERE id = ?1", [nid])?;
    }
    for cid in &graves.cards {
        conn.execute("DELETE FROM cards WHERE id = ?1", [cid])?;
    }
    for did in &graves.decks {
        conn.execute("DELETE FROM decks WHERE id = ?1", [did])
            .or_else(|_| {
                // Legacy schema: remove from decks JSON in col
                Ok::<_, rusqlite::Error>(0)
            })?;
    }
    Ok(())
}

fn apply_tags(conn: &Connection, tags: &[String], usn: i64) -> Result<()> {
    for tag in tags {
        // Upsert tag
        conn.execute(
            "INSERT OR REPLACE INTO tags (tag, usn) VALUES (?1, ?2)",
            rusqlite::params![tag, usn],
        )
        .ok(); // tags table may not exist in very old collections
    }
    Ok(())
}

/// Apply a chunk of notes/cards/revlog from the server.
/// Notes are JSON arrays: [id, guid, mid, mod, usn, tags, flds, sfld, csum, flags, data]
/// Cards are JSON arrays: [id, nid, did, ord, mtime, usn, type, queue, due, ivl, factor, reps, lapses, left, odue, odid, flags, data]
fn apply_chunk(conn: &Connection, chunk: &Chunk) -> Result<()> {
    for note in &chunk.notes {
        let arr = note.as_array().context("note entry is not an array")?;
        if arr.len() < 11 {
            continue;
        }
        let id = arr[0].as_i64().unwrap_or(0);
        let guid = arr[1].as_str().unwrap_or("");
        let mid = arr[2].as_i64().unwrap_or(0);
        let mod_time = arr[3].as_i64().unwrap_or(0);
        let usn = arr[4].as_i64().unwrap_or(0);
        let tags = arr[5].as_str().unwrap_or("");
        let flds = arr[6].as_str().unwrap_or("");
        // sfld and csum from wire are empty strings in modern protocol;
        // we need to compute them ourselves
        let sort_field = flds.split('\x1f').next().unwrap_or("");
        let csum = {
            use sha1::{Digest, Sha1};
            let mut hasher = Sha1::new();
            hasher.update(sort_field.trim().as_bytes());
            let hash = hasher.finalize();
            let hex = format!("{:x}", hash);
            u32::from_str_radix(&hex[..8], 16).unwrap_or(0) as i64
        };
        let flags = arr[9].as_i64().unwrap_or(0);
        let data = arr[10].as_str().unwrap_or("");

        conn.execute(
            "INSERT OR REPLACE INTO notes (id, guid, mid, mod, usn, tags, flds, sfld, csum, flags, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![id, guid, mid, mod_time, usn, tags, flds, sort_field, csum, flags, data],
        )?;
    }

    for card in &chunk.cards {
        let arr = card.as_array().context("card entry is not an array")?;
        if arr.len() < 18 {
            continue;
        }
        let id = arr[0].as_i64().unwrap_or(0);
        let nid = arr[1].as_i64().unwrap_or(0);
        let did = arr[2].as_i64().unwrap_or(0);
        let ord = arr[3].as_i64().unwrap_or(0);
        let mod_time = arr[4].as_i64().unwrap_or(0);
        let usn = arr[5].as_i64().unwrap_or(0);
        let ctype = arr[6].as_i64().unwrap_or(0);
        let queue = arr[7].as_i64().unwrap_or(0);
        let due = arr[8].as_i64().unwrap_or(0);
        let ivl = arr[9].as_i64().unwrap_or(0);
        let factor = arr[10].as_i64().unwrap_or(0);
        let reps = arr[11].as_i64().unwrap_or(0);
        let lapses = arr[12].as_i64().unwrap_or(0);
        let left = arr[13].as_i64().unwrap_or(0);
        let odue = arr[14].as_i64().unwrap_or(0);
        let odid = arr[15].as_i64().unwrap_or(0);
        let flags = arr[16].as_i64().unwrap_or(0);
        let data = arr[17].as_str().unwrap_or("");

        conn.execute(
            "INSERT OR REPLACE INTO cards (id, nid, did, ord, mod, usn, type, queue, due, ivl, factor, reps, lapses, left, odue, odid, flags, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            rusqlite::params![id, nid, did, ord, mod_time, usn, ctype, queue, due, ivl, factor, reps, lapses, left, odue, odid, flags, data],
        )?;
    }

    for revlog in &chunk.revlog {
        let arr = revlog.as_array().context("revlog entry is not an array")?;
        if arr.len() < 9 {
            continue;
        }
        let id = arr[0].as_i64().unwrap_or(0);
        let cid = arr[1].as_i64().unwrap_or(0);
        let usn = arr[2].as_i64().unwrap_or(0);
        let ease = arr[3].as_i64().unwrap_or(0);
        let ivl = arr[4].as_i64().unwrap_or(0);
        let last_ivl = arr[5].as_i64().unwrap_or(0);
        let factor = arr[6].as_i64().unwrap_or(0);
        let time = arr[7].as_i64().unwrap_or(0);
        let typ = arr[8].as_i64().unwrap_or(0);

        conn.execute(
            "INSERT OR IGNORE INTO revlog (id, cid, usn, ease, ivl, lastIvl, factor, time, type) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![id, cid, usn, ease, ivl, last_ivl, factor, time, typ],
        )?;
    }

    Ok(())
}

// ── Building outgoing data ─────────────────────────────────────────

/// Get IDs of notes/cards pending sync (usn = -1).
fn pending_note_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM notes WHERE usn = -1")?;
    let ids: Vec<i64> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ids)
}

fn pending_card_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM cards WHERE usn = -1")?;
    let ids: Vec<i64> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Build a NoteEntry JSON array from a local note, setting usn to server_usn.
fn note_to_wire(conn: &Connection, id: i64, server_usn: i64) -> Result<Value> {
    conn.query_row(
        "SELECT id, guid, mid, mod, tags, flds, flags, data FROM notes WHERE id = ?1",
        [id],
        |row| {
            let id: i64 = row.get(0)?;
            let guid: String = row.get(1)?;
            let mid: i64 = row.get(2)?;
            let mod_time: i64 = row.get(3)?;
            let tags: String = row.get(4)?;
            let flds: String = row.get(5)?;
            let flags: i64 = row.get(6)?;
            let data: String = row.get(7)?;
            Ok(serde_json::json!([id, guid, mid, mod_time, server_usn, tags, flds, "", "", flags, data]))
        },
    )
    .context("reading note for sync")
}

/// Build a CardEntry JSON array from a local card, setting usn to server_usn.
fn card_to_wire(conn: &Connection, id: i64, server_usn: i64) -> Result<Value> {
    conn.query_row(
        "SELECT id, nid, did, ord, mod, type, queue, due, ivl, factor, reps, lapses, left, odue, odid, flags, data FROM cards WHERE id = ?1",
        [id],
        |row| {
            let id: i64 = row.get(0)?;
            let nid: i64 = row.get(1)?;
            let did: i64 = row.get(2)?;
            let ord: i64 = row.get(3)?;
            let mod_time: i64 = row.get(4)?;
            let ctype: i64 = row.get(5)?;
            let queue: i64 = row.get(6)?;
            let due: i64 = row.get(7)?;
            let ivl: i64 = row.get(8)?;
            let factor: i64 = row.get(9)?;
            let reps: i64 = row.get(10)?;
            let lapses: i64 = row.get(11)?;
            let left: i64 = row.get(12)?;
            let odue: i64 = row.get(13)?;
            let odid: i64 = row.get(14)?;
            let flags: i64 = row.get(15)?;
            let data: String = row.get(16)?;
            Ok(serde_json::json!([id, nid, did, ord, mod_time, server_usn, ctype, queue, due, ivl, factor, reps, lapses, left, odue, odid, flags, data]))
        },
    )
    .context("reading card for sync")
}

/// Update local USNs after successful sync.
fn update_pending_usns(conn: &Connection, server_usn: i64) -> Result<()> {
    conn.execute(
        "UPDATE notes SET usn = ?1 WHERE usn = -1",
        [server_usn],
    )?;
    conn.execute(
        "UPDATE cards SET usn = ?1 WHERE usn = -1",
        [server_usn],
    )?;
    Ok(())
}

// ── Sanity check ───────────────────────────────────────────────────

fn has_table(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .map(|c| c > 0)
    .unwrap_or(false)
}

fn sanity_counts(conn: &Connection) -> Result<SanityCheckCounts> {
    let cards: u32 = conn.query_row("SELECT count() FROM cards", [], |r| r.get(0))?;
    let notes: u32 = conn.query_row("SELECT count() FROM notes", [], |r| r.get(0))?;
    let revlog: u32 = conn.query_row("SELECT count() FROM revlog", [], |r| r.get(0))?;
    let graves: u32 = if has_table(conn, "graves") {
        conn.query_row("SELECT count() FROM graves", [], |r| r.get(0))?
    } else {
        0
    };
    let notetypes: u32 = if has_table(conn, "notetypes") {
        conn.query_row("SELECT count() FROM notetypes", [], |r| r.get(0))?
    } else {
        let json: String = conn.query_row("SELECT models FROM col", [], |r| r.get(0))?;
        let m: std::collections::HashMap<String, Value> = serde_json::from_str(&json)?;
        m.len() as u32
    };
    let decks: u32 = if has_table(conn, "decks") {
        conn.query_row("SELECT count() FROM decks", [], |r| r.get(0))?
    } else {
        let json: String = conn.query_row("SELECT decks FROM col", [], |r| r.get(0))?;
        let d: std::collections::HashMap<String, Value> = serde_json::from_str(&json)?;
        d.len() as u32
    };
    let deck_config: u32 = if has_table(conn, "deck_config") {
        conn.query_row("SELECT count() FROM deck_config", [], |r| r.get(0))?
    } else {
        let json: String = conn.query_row("SELECT dconf FROM col", [], |r| r.get(0))?;
        let d: std::collections::HashMap<String, Value> = serde_json::from_str(&json)?;
        d.len() as u32
    };

    // Due counts are zeroed by server before comparison, so send 0s
    Ok(SanityCheckCounts(
        (0, 0, 0),
        cards,
        notes,
        revlog,
        graves,
        notetypes,
        decks,
        deck_config,
    ))
}

// ── Main sync entry point ──────────────────────────────────────────

fn local_collection_path() -> Result<PathBuf> {
    let data_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ankiweb-cli");
    std::fs::create_dir_all(&data_dir)?;
    Ok(data_dir.join("collection.anki2"))
}

fn open_local_collection(path: &Path) -> Result<Connection> {
    crate::collection::open_local(path)
}

/// Ensure a local collection exists. Returns the path.
pub async fn ensure_local_collection_path(config: &SyncConfig) -> Result<PathBuf> {
    ensure_local_collection(config).await
}

/// Ensure a local collection exists. Downloads from AnkiWeb if needed.
async fn ensure_local_collection(config: &SyncConfig) -> Result<PathBuf> {
    let path = local_collection_path()?;
    if !path.exists() {
        eprintln!("No local collection found. Downloading from AnkiWeb...");
        let data = sync::download_collection(config).await?;
        std::fs::write(&path, &data)?;
        let conn = open_local_collection(&path)?;
        mark_downloaded(&conn)?;
        eprintln!("Local collection saved to {}", path.display());
    }
    Ok(path)
}

/// Determine sync action needed.
enum SyncAction {
    NoChanges,
    NormalSync { local_is_newer: bool },
    FullSyncRequired,
}

fn determine_sync_action(local: &LocalMeta, server_mod: i64, server_schema: i64) -> SyncAction {
    if local.modification == server_mod {
        SyncAction::NoChanges
    } else if local.schema != server_schema {
        SyncAction::FullSyncRequired
    } else {
        SyncAction::NormalSync {
            local_is_newer: local.modification > server_mod,
        }
    }
}

/// Perform a normal sync to push/pull changes.
/// Returns Ok(true) if normal sync succeeded, Ok(false) if full sync is required.
async fn do_normal_sync(
    config: &SyncConfig,
    collection_path: &Path,
) -> Result<bool> {
    let conn = open_local_collection(collection_path)?;
    let local = read_local_meta(&conn)?;

    // Establish server session
    let session = sync::establish_session(config).await?;

    // Parse server meta
    let server_meta: Value = serde_json::from_slice(&session.meta_data)?;
    let server_mod = server_meta["mod"].as_i64().unwrap_or(0);
    let server_schema = server_meta["scm"].as_i64().unwrap_or(0);
    let server_usn = server_meta["usn"].as_i64().unwrap_or(0);
    let server_ts = server_meta["ts"].as_i64().unwrap_or(0);

    // Check clock
    let local_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    if (server_ts - local_ts).abs() > 300 {
        return Err(anyhow!("clock is off by more than 5 minutes"));
    }

    let should_continue = server_meta["cont"].as_bool().unwrap_or(true);
    if !should_continue {
        let msg = server_meta["msg"].as_str().unwrap_or("server refused to continue");
        return Err(anyhow!("{msg}"));
    }

    match determine_sync_action(&local, server_mod, server_schema) {
        SyncAction::NoChanges => {
            eprintln!("Collection is up to date, no sync needed.");
            return Ok(true);
        }
        SyncAction::FullSyncRequired => {
            eprintln!("Schema changed, full sync required.");
            return Ok(false);
        }
        SyncAction::NormalSync { local_is_newer } => {
            eprintln!(
                "Normal sync: local_is_newer={}, server_usn={}",
                local_is_newer, server_usn
            );

            // 1. Start
            let start_req = StartRequest {
                client_usn: local.usn,
                local_is_newer,
            };
            let start_body = serde_json::to_vec(&start_req)?;
            let start_result = session.request("start", &start_body).await?;
            let server_graves: Graves = serde_json::from_slice(&start_result)?;

            // Apply server graves locally
            if !server_graves.cards.is_empty()
                || !server_graves.notes.is_empty()
                || !server_graves.decks.is_empty()
            {
                tracing::info!(
                    cards = server_graves.cards.len(),
                    notes = server_graves.notes.len(),
                    decks = server_graves.decks.len(),
                    "applying server graves"
                );
                apply_graves(&conn, &server_graves)?;
            }

            // 2. Send our graves (none)
            let our_graves = ApplyGravesRequest {
                chunk: Graves::default(),
            };
            let graves_body = serde_json::to_vec(&our_graves)?;
            session.request("applyGraves", &graves_body).await?;

            // 3. Unchunked changes
            let our_changes = ApplyChangesRequest {
                changes: UnchunkedChanges {
                    models: vec![],
                    decks: (vec![], vec![]),
                    tags: vec![],
                    conf: if local_is_newer {
                        // Send config if we're newer
                        None // We don't modify config, so skip
                    } else {
                        None
                    },
                    crt: None,
                },
            };
            let changes_body = serde_json::to_vec(&our_changes)?;
            let changes_result = session.request("applyChanges", &changes_body).await?;
            let server_changes: UnchunkedChanges = serde_json::from_slice(&changes_result)?;

            // Apply server's unchunked changes
            if !server_changes.models.is_empty()
                || !server_changes.decks.0.is_empty()
                || !server_changes.decks.1.is_empty()
            {
                // Complex changes we can't easily apply to new-format DB
                // Fall back to full sync
                eprintln!(
                    "Server has notetype/deck changes ({} notetypes, {} decks, {} deck_config). Falling back to full sync.",
                    server_changes.models.len(),
                    server_changes.decks.0.len(),
                    server_changes.decks.1.len(),
                );
                // Abort the sync session
                session.request("abort", b"{}").await.ok();
                return Ok(false);
            }
            apply_tags(&conn, &server_changes.tags, server_usn)?;

            // 4. Receive chunks from server
            loop {
                let chunk_result = session.request("chunk", b"{}").await?;
                let chunk: Chunk = serde_json::from_slice(&chunk_result)?;
                let done = chunk.done;

                if !chunk.notes.is_empty() || !chunk.cards.is_empty() || !chunk.revlog.is_empty() {
                    tracing::info!(
                        notes = chunk.notes.len(),
                        cards = chunk.cards.len(),
                        revlog = chunk.revlog.len(),
                        "applying server chunk"
                    );
                    apply_chunk(&conn, &chunk)?;
                }

                if done {
                    break;
                }
            }

            // 5. Send our chunks
            let pending_notes = pending_note_ids(&conn)?;
            let pending_cards = pending_card_ids(&conn)?;

            let mut our_chunk = Chunk {
                done: true,
                notes: vec![],
                cards: vec![],
                revlog: vec![],
            };

            for nid in &pending_notes {
                our_chunk.notes.push(note_to_wire(&conn, *nid, server_usn)?);
            }
            for cid in &pending_cards {
                our_chunk.cards.push(card_to_wire(&conn, *cid, server_usn)?);
            }

            tracing::info!(
                notes = our_chunk.notes.len(),
                cards = our_chunk.cards.len(),
                "sending our chunk"
            );

            let chunk_body = serde_json::to_vec(&ApplyChunkRequest { chunk: our_chunk })?;
            session.request("applyChunk", &chunk_body).await?;

            // Update local USNs before sanity check
            update_pending_usns(&conn, server_usn)?;

            // 6. Sanity check
            let counts = sanity_counts(&conn)?;
            let sanity_body = serde_json::to_vec(&SanityCheckRequest { client: counts })?;
            let sanity_result = session.request("sanityCheck2", &sanity_body).await?;
            let sanity_resp: SanityCheckResponse = serde_json::from_slice(&sanity_result)?;

            if sanity_resp.status != "ok" {
                eprintln!("Sanity check failed! Falling back to full sync.");
                session.request("abort", b"{}").await.ok();
                return Ok(false);
            }

            // 7. Finish
            let finish_result = session.request("finish", b"{}").await?;
            let new_mod: i64 = serde_json::from_slice(&finish_result)?;

            // Update local meta
            update_local_meta_after_sync(&conn, new_mod, server_usn + 1)?;

            eprintln!("Normal sync complete.");
            return Ok(true);
        }
    }
}

/// Add a note using normal sync (incremental, safe).
/// Falls back to full sync if needed.
pub async fn add_note_normal(
    config: &SyncConfig,
    deck: &str,
    notetype: &str,
    field_values: &[String],
    tags: &str,
    due_in_secs: Option<i64>,
) -> Result<i64> {
    // Ensure local collection exists
    let collection_path = ensure_local_collection(config).await?;

    // Add note to local collection
    let note_id = {
        let conn = open_local_collection(&collection_path)?;
        let deck_id = crate::collection::find_or_create_deck(&conn, deck)?;
        let (model_id, _fields) = crate::collection::find_model_by_name(&conn, notetype)?;
        crate::collection::add_note_with_fields(&conn, deck_id, model_id, field_values, tags, due_in_secs)?
    };

    // Try normal sync
    match do_normal_sync(config, &collection_path).await {
        Ok(true) => {
            eprintln!("Note synced successfully via normal sync.");
            return Ok(note_id);
        }
        Ok(false) => {
            eprintln!("Normal sync not possible, falling back to full upload...");
        }
        Err(e) => {
            eprintln!("Normal sync failed: {e}. Falling back to full upload...");
        }
    }

    // Fallback: full upload
    let data = crate::collection::read_collection(&collection_path)?;
    sync::upload_collection(config, &data).await?;
    eprintln!("Uploaded via full sync.");

    Ok(note_id)
}

/// List decks using local collection (syncs first if needed).
pub async fn list_decks_with_sync(config: &SyncConfig) -> Result<Vec<(i64, String)>> {
    let collection_path = ensure_local_collection(config).await?;
    // Pull latest changes
    do_normal_sync(config, &collection_path).await.ok();
    let conn = open_local_collection(&collection_path)?;
    crate::collection::list_decks(&conn)
}

/// List notetypes using local collection (syncs first if needed).
pub async fn list_notetypes_with_sync(config: &SyncConfig) -> Result<Vec<(i64, String, Vec<String>)>> {
    let collection_path = ensure_local_collection(config).await?;
    do_normal_sync(config, &collection_path).await.ok();
    let conn = open_local_collection(&collection_path)?;
    crate::collection::list_notetypes(&conn)
}
