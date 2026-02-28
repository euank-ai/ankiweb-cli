mod collection;
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
    /// Add a note to a deck
    AddNote {
        /// Deck name (created if it doesn't exist)
        #[arg(long)]
        deck: String,

        /// Front of the card
        #[arg(long)]
        front: String,

        /// Back of the card
        #[arg(long)]
        back: String,

        /// Space-separated tags
        #[arg(long, default_value = "")]
        tags: String,
    },

    /// Download a backup of the collection
    Backup {
        /// Output file path (default: collection-<timestamp>.anki2)
        #[arg(long, short)]
        output: Option<PathBuf>,
    },

    /// List all decks
    ListDecks,
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
            front,
            back,
            tags,
        } => {
            eprintln!("Downloading collection from AnkiWeb...");
            let data = sync::download_collection(&sync_config).await?;

            let tmp = tempfile::NamedTempFile::new()?;
            let db_path = tmp.path().to_path_buf();
            let conn = collection::open_collection(&data, &db_path)?;

            let deck_id = collection::find_or_create_deck(&conn, deck)?;
            let model_id = collection::find_basic_model(&conn)?;
            let note_id = collection::add_note(&conn, deck_id, model_id, front, back, tags)?;

            // Close connection before reading file
            drop(conn);

            eprintln!("Uploading modified collection to AnkiWeb...");
            let modified = collection::read_collection(&db_path)?;
            sync::upload_collection(&sync_config, &modified).await?;

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

        Commands::ListDecks => {
            eprintln!("Downloading collection from AnkiWeb...");
            let data = sync::download_collection(&sync_config).await?;

            let tmp = tempfile::NamedTempFile::new()?;
            let conn = collection::open_collection(&data, tmp.path())?;
            let decks = collection::list_decks(&conn)?;

            for (id, name) in &decks {
                println!("{id}\t{name}");
            }
        }
    }

    Ok(())
}
