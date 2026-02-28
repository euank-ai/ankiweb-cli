# ankiweb-cli

CLI tool for interacting with AnkiWeb directly — no Anki desktop or AnkiConnect required.

## Features

- **Add notes** to any deck with arbitrary fields (supports complex note types like Core2k)
- **List decks** in your collection
- **List note types** and their fields
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

Simple (Basic note type):
```bash
ankiweb-cli add-note --deck "My Deck" --front "What is Rust?" --back "A systems programming language"
```

With a specific note type and custom fields:
```bash
ankiweb-cli add-note --deck "Japanese::Core2k" --notetype "Core 2000" \
  -f "Vocabulary-Kanji=人" \
  -f "Vocabulary-Kana=ひと" \
  -f "Vocabulary-English=person" \
  -f "Vocabulary-Pos=noun" \
  -f "Sentence-Kanji=あの人は誰ですか。" \
  -f "Sentence-English=Who is that person?" \
  --tags "core2k japanese"
```

### List note types and fields

See what note types exist and what fields they have:
```bash
ankiweb-cli list-notetypes
```

Example output:
```
1234567890  Basic
  [0] Front
  [1] Back
1234567891  Core 2000
  [0] Vocabulary-Kanji
  [1] Vocabulary-Kana
  [2] Vocabulary-English
  [3] Vocabulary-Audio
  [4] Vocabulary-Pos
  [5] Sentence-Kanji
  [6] Sentence-Kana
  [7] Sentence-English
  [8] Sentence-Audio
  [9] Image
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
3. Inserts the note and card(s) (one per card template in the note type)
4. Uploads the modified collection back

**Important:** Make sure to sync any pending changes from your Anki clients before using `add-note`, as the upload replaces the server collection.

## License

MIT
