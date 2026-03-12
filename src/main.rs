mod collection;
mod normal_sync;
mod sync;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::path::PathBuf;

use sync::SyncConfig;

#[derive(Parser)]
#[command(name = "ankiweb-cli", version, about = "CLI for AnkiWeb: add notes, list decks, backup collections")]
struct Cli {
    /// AnkiWeb username/email
    #[arg(long, global = true)]
    username: Option<String>,

    /// AnkiWeb password
    #[arg(long, global = true)]
    password: Option<String>,

    /// Path to config file (default: ~/.config/ankiweb-cli/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Add a note to a deck.
    ///
    /// Fields can be specified as key=value pairs, or use --front/--back
    /// for simple Basic notes.
    ///
    /// Examples:
    ///   ankiweb-cli add-note --deck "Japanese" --front "猫" --back "cat"
    ///   ankiweb-cli add-note --deck "Core2k" --notetype "Core 2000" \
    ///     -f "Vocabulary-Kanji=人" -f "Vocabulary-Kana=ひと" \
    ///     -f "Vocabulary-English=person"
    AddNote {
        /// Deck name (created if it doesn't exist)
        #[arg(long)]
        deck: String,

        /// Note type / model name (default: "Basic")
        #[arg(long, default_value = "Basic")]
        notetype: String,

        /// Front of the card (shorthand for Basic notes)
        #[arg(long)]
        front: Option<String>,

        /// Back of the card (shorthand for Basic notes)
        #[arg(long)]
        back: Option<String>,

        /// Field values as "FieldName=value" pairs. Repeatable.
        /// Use this for note types with custom fields.
        #[arg(short = 'f', long = "field", value_name = "NAME=VALUE")]
        fields: Vec<String>,

        /// Space-separated tags
        #[arg(long, default_value = "")]
        tags: String,

        /// Make the card due for review after this duration.
        /// Examples: "5m", "2h", "3d", "1w"
        /// (minutes, hours, days, weeks)
        #[arg(long)]
        due_in: Option<String>,
    },

    /// Download a backup of the collection
    Backup {
        /// Output file path (default: collection-<timestamp>.anki2)
        #[arg(long, short)]
        output: Option<PathBuf>,
    },

    /// Reschedule an existing card by setting a new due date.
    ///
    /// Find cards by note content (searches the sort field).
    ///
    /// Examples:
    ///   ankiweb-cli reschedule --query "猫" --due-in "3d"
    ///   ankiweb-cli reschedule --note-id 1234567890 --due-in "1w"
    Reschedule {
        /// Search for cards whose note sort field contains this text
        #[arg(long, group = "target")]
        query: Option<String>,

        /// Target a specific note by ID
        #[arg(long, group = "target")]
        note_id: Option<i64>,

        /// New due date as a duration from now.
        /// Examples: "5m", "2h", "3d", "1w"
        #[arg(long)]
        due_in: String,
    },

    /// List all decks
    ListDecks,

    /// List note types (models) and their fields
    ListNotetypes,
}

#[derive(Deserialize, Default)]
struct Config {
    username: Option<String>,
    password: Option<String>,
}

fn load_config(path: Option<&PathBuf>) -> Config {
    let path = match path {
        Some(p) => p.clone(),
        None => {
            let Some(config_dir) = dirs::config_dir() else {
                return Config::default();
            };
            config_dir.join("ankiweb-cli").join("config.toml")
        }
    };

    match std::fs::read_to_string(&path) {
        Ok(contents) => toml::from_str(&contents).unwrap_or_else(|e| {
            eprintln!("warning: failed to parse {}: {e}", path.display());
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

fn resolve_sync_config(cli: &Cli) -> Result<SyncConfig> {
    let file_config = load_config(cli.config.as_ref());

    let username = cli
        .username
        .clone()
        .or(file_config.username)
        .context("username required (--username or config file)")?;
    let password = cli
        .password
        .clone()
        .or(file_config.password)
        .context("password required (--password or config file)")?;

    Ok(SyncConfig {
        username,
        password,
        endpoint: None,
    })
}

/// Parse --field args and --front/--back into an ordered field list.
fn resolve_fields(
    front: &Option<String>,
    back: &Option<String>,
    field_args: &[String],
    notetype: &str,
    model_fields: &[String],
) -> Result<Vec<String>> {
    if !field_args.is_empty() {
        // Build from -f args, matching against model field order
        let mut values: Vec<String> = vec![String::new(); model_fields.len()];
        for arg in field_args {
            let (name, value) = arg
                .split_once('=')
                .context(format!("field must be NAME=VALUE, got: {arg}"))?;
            let idx = model_fields
                .iter()
                .position(|f| f == name)
                .context(format!(
                    "field '{}' not found in notetype '{}'. Available fields: {}",
                    name,
                    notetype,
                    model_fields.join(", ")
                ))?;
            values[idx] = value.to_string();
        }
        Ok(values)
    } else if let (Some(f), Some(b)) = (front, back) {
        // Simple front/back mode
        if model_fields.len() < 2 {
            anyhow::bail!(
                "notetype '{}' has {} field(s), need at least 2 for --front/--back",
                notetype,
                model_fields.len()
            );
        }
        let mut values = vec![String::new(); model_fields.len()];
        values[0] = f.clone();
        values[1] = b.clone();
        Ok(values)
    } else {
        anyhow::bail!("provide either --front and --back, or -f FIELD=VALUE pairs")
    }
}

/// Parse a human-friendly duration string into seconds.
/// Supports: 30s, 5m, 2h, 3d, 1w (and combinations like "1d12h")
fn parse_duration(s: &str) -> Result<i64> {
    let mut total: i64 = 0;
    let mut num_buf = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num_buf.push(c);
        } else {
            let n: i64 = num_buf
                .parse()
                .context(format!("invalid duration: '{s}'"))?;
            num_buf.clear();
            let multiplier = match c {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                'd' => 86400,
                'w' => 604800,
                _ => anyhow::bail!("unknown duration unit '{c}' in '{s}' (use s/m/h/d/w)"),
            };
            total += n * multiplier;
        }
    }
    if !num_buf.is_empty() {
        // bare number defaults to seconds
        total += num_buf.parse::<i64>().context("invalid duration")?;
    }
    if total == 0 {
        anyhow::bail!("duration must be > 0");
    }
    Ok(total)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let sync_config = resolve_sync_config(&cli)?;

    match &cli.command {
        Commands::AddNote {
            deck,
            notetype,
            front,
            back,
            fields,
            tags,
            due_in,
        } => {
            // We need to know the model fields to resolve --front/--back vs -f args.
            // Ensure local collection exists first so we can look them up.
            let collection_path = normal_sync::ensure_local_collection_path(&sync_config).await?;
            let conn = collection::open_local(&collection_path)?;
            let (_model_id, model_fields) = collection::find_model_by_name(&conn, notetype)?;
            drop(conn);

            let field_values = resolve_fields(front, back, fields, notetype, &model_fields)?;
            let due_secs = due_in.as_deref().map(parse_duration).transpose()?;

            let note_id = normal_sync::add_note_normal(
                &sync_config,
                deck,
                notetype,
                &field_values,
                tags,
                due_secs,
            )
            .await?;

            println!("Added note {note_id} to deck \"{deck}\"");
        }

        Commands::Backup { output } => {
            eprintln!("Downloading collection from AnkiWeb...");
            let data = sync::download_collection(&sync_config).await?;

            let path = output.clone().unwrap_or_else(|| {
                let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                PathBuf::from(format!("collection-{ts}.anki2"))
            });

            std::fs::write(&path, &data)?;
            println!("Saved {} bytes to {}", data.len(), path.display());
        }

        Commands::Reschedule { query, note_id, due_in } => {
            let due_secs = parse_duration(due_in)?;
            let count = normal_sync::reschedule(
                &sync_config,
                query.as_deref(),
                *note_id,
                due_secs,
            )
            .await?;
            println!("Rescheduled {count} card(s)");
        }

        Commands::ListDecks => {
            let decks = normal_sync::list_decks_with_sync(&sync_config).await?;
            for (id, name) in &decks {
                println!("{id}\t{name}");
            }
        }

        Commands::ListNotetypes => {
            let notetypes = normal_sync::list_notetypes_with_sync(&sync_config).await?;
            for (id, name, fields) in &notetypes {
                println!("{id}\t{name}");
                for (i, field) in fields.iter().enumerate() {
                    println!("  [{i}] {field}");
                }
            }
        }
    }

    Ok(())
}
