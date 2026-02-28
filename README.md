# ankiweb-cli

CLI tool for interacting with AnkiWeb directly — no Anki desktop or AnkiConnect required.

## Features

- **Add notes** to any deck (creates the deck if needed)
- **List decks** in your collection
- **Backup** your full collection

## Install

```bash
cargo install --path .
```

## Usage

### Authentication

Via CLI flags:
```bash
ankiweb-cli --username user@example.com --password secret <command>
```

Via config file (`~/.config/ankiweb-cli/config.toml`):
```toml
username = "user@example.com"
password = "secret"
```

Or specify a custom config path:
```bash
ankiweb-cli --config /path/to/config.toml <command>
```

### Add a note

```bash
ankiweb-cli add-note --deck "My Deck" --front "What is Rust?" --back "A systems programming language"
```

With tags:
```bash
ankiweb-cli add-note --deck "Japanese" --front "猫" --back "cat" --tags "japanese animals"
```

### List decks

```bash
ankiweb-cli list-decks
```

### Backup collection

```bash
ankiweb-cli backup
ankiweb-cli backup --output my-backup.anki2
```

## How it works

This tool communicates directly with AnkiWeb using Anki's sync protocol (v11). For `add-note`, it:

1. Downloads your full collection from AnkiWeb
2. Opens the SQLite database
3. Inserts the note and card
4. Uploads the modified collection back

**Important:** Make sure to sync any pending changes from your Anki clients before using `add-note`, as the upload replaces the server collection. The tool uses the "full sync" (download → modify → upload) approach, which is equivalent to forcing an upload from a client.

## License

MIT
