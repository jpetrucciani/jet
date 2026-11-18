//! Ghostty's config format (`key = value` lines, repeated keys append),
//! translated into jet's TOML layout so the layers merge as plain tables.
//! Theme files are the same format.

use toml::{Table, Value};

use crate::{keybinds, parse_color};

/// `(key, value)` pairs in file order.
pub(crate) fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            Some((key.trim().to_owned(), value.to_owned()))
        })
        .collect()
}

/// Inline `[ghostty]` keys as the same pairs; an array repeats its key.
pub(crate) fn from_table(table: &Table) -> Vec<(String, String)> {
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut pairs = Vec::new();
    for (key, value) in table {
        match value {
            Value::Array(items) => pairs.extend(items.iter().map(|v| (key.clone(), text(v)))),
            value => pairs.push((key.clone(), text(value))),
        }
    }
    pairs
}

/// jet's table for `pairs`, and the keys (with values that couldn't be
/// used) that were skipped.
pub(crate) fn translate(pairs: &[(String, String)]) -> (Table, Vec<String>) {
    let mut out = Table::new();
    let mut skipped = Vec::new();
    let (mut pad_x, mut pad_y) = (None, None);
    for (key, value) in pairs {
        // An empty value resets a key to its default: leave it unset.
        if value.is_empty() || key == "config-file" {
            continue;
        }
        let color = || parse_color(value).map(|_| Value::String(value.clone()));
        let ok = match key.as_str() {
            "font-family" => {
                // Later families are fallbacks in Ghostty; jet takes the first.
                if !has(&out, "font", "family") {
                    set(&mut out, "font", "family", Value::String(value.clone()));
                }
                true
            }
            "font-size" => value
                .parse()
                .map(|size| set(&mut out, "font", "size", Value::Float(size)))
                .is_ok(),
            "font-feature" => {
                push(&mut out, "font", "features", Value::String(value.clone()));
                true
            }
            "theme" => {
                set(&mut out, "theme", "name", Value::String(theme_name(value)));
                true
            }
            "palette" => match value.split_once('=') {
                Some((index, c)) if index.trim().parse::<u8>().is_ok() => {
                    let c = c.trim();
                    let valid = parse_color(c).is_some();
                    let palette = table(&mut out, "colors").and_then(|t| table(t, "palette"));
                    if let Some(palette) = palette.filter(|_| valid) {
                        palette.insert(index.trim().to_owned(), Value::String(c.to_owned()));
                    }
                    valid
                }
                _ => false,
            },
            "background"
            | "foreground"
            | "cursor-color"
            | "selection-background"
            | "selection-foreground" => color()
                .map(|c| {
                    let name = key.strip_suffix("-color").unwrap_or(key);
                    set(&mut out, "colors", name, c);
                })
                .is_some(),
            "background-opacity" => value
                .parse::<f64>()
                .ok()
                .filter(|o| (0.0..=1.0).contains(o))
                .map(|o| set(&mut out, "window", "opacity", Value::Float(o)))
                .is_some(),
            "window-padding-x" => {
                pad_x = first_number(value);
                pad_x.is_some()
            }
            "window-padding-y" => {
                pad_y = first_number(value);
                pad_y.is_some()
            }
            "keybind" if value == "clear" => {
                set(&mut out, "keybinds", "defaults", Value::Boolean(false));
                true
            }
            // Prefixed (`global:`, `all:`...) and sequence (`a>b`) bindings
            // aren't supported, and fail to parse as a chord.
            "keybind" => match value.split_once('=') {
                Some((chord, action)) if keybinds::parse(chord, action).is_ok() => {
                    let action = Value::String(action.trim().to_owned());
                    set(&mut out, "keybinds", chord.trim(), action);
                    true
                }
                _ => false,
            },
            "command" => {
                set(&mut out, "shell", "command", Value::String(value.clone()));
                true
            }
            "cursor-style" => {
                let style = match value.as_str() {
                    "block" | "block_hollow" => Some("block"),
                    "bar" => Some("bar"),
                    "underline" => Some("underline"),
                    _ => None,
                };
                style
                    .map(|s| set(&mut out, "cursor", "style", Value::String(s.into())))
                    .is_some()
            }
            "cursor-style-blink" => value
                .parse()
                .map(|blink| set(&mut out, "cursor", "blink", Value::Boolean(blink)))
                .is_ok(),
            // No prompt to ask with, so asking is denying.
            "clipboard-write" => {
                let on = match value.as_str() {
                    "allow" => Some(true),
                    "deny" | "ask" => Some(false),
                    _ => None,
                };
                on.map(|on| set(&mut out, "terminal", "clipboard-write", Value::Boolean(on)))
                    .is_some()
            }
            "desktop-notifications" => {
                let on = match value.as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                };
                on.map(|on| set(&mut out, "terminal", "notifications", Value::Boolean(on)))
                    .is_some()
            }
            "copy-on-select" => {
                let on = match value.as_str() {
                    "true" | "clipboard" => Some(true),
                    "false" => Some(false),
                    _ => None,
                };
                on.map(|on| set(&mut out, "terminal", "copy-on-select", Value::Boolean(on)))
                    .is_some()
            }
            "clipboard-paste-protection" => value
                .parse()
                .map(|on| set(&mut out, "terminal", "paste-protection", Value::Boolean(on)))
                .is_ok(),
            "confirm-close-surface" => {
                // `always` asks even at a prompt; jet asks when it matters.
                let on = match value.as_str() {
                    "true" | "always" => Some(true),
                    "false" => Some(false),
                    _ => None,
                };
                on.map(|on| set(&mut out, "window", "confirm-close", Value::Boolean(on)))
                    .is_some()
            }
            "unfocused-split-opacity" => value
                .parse::<f64>()
                .map(|o| {
                    // Ghostty clamps to 0.15..=1.
                    let o = o.clamp(0.15, 1.0);
                    set(
                        &mut out,
                        "window",
                        "unfocused-split-opacity",
                        Value::Float(o),
                    );
                })
                .is_ok(),
            _ => false,
        };
        if !ok {
            skipped.push(format!("{key} = {value}"));
        }
    }
    // jet pads evenly; x wins, as the wider side matters more.
    if let Some(pad) = pad_x.or(pad_y) {
        set(&mut out, "window", "padding", Value::Integer(pad));
    }
    (out, skipped)
}

/// Ghostty's `light:A,dark:B` pairs: jet doesn't follow the system theme
/// yet, so the dark one.
fn theme_name(value: &str) -> String {
    let dark = value
        .split(',')
        .find_map(|part| part.trim().strip_prefix("dark:"));
    dark.unwrap_or(value).trim().to_owned()
}

fn first_number(value: &str) -> Option<i64> {
    value.split(',').next()?.trim().parse().ok()
}

/// The subtable at `key`, made if missing (or replacing a non-table, which
/// translation never writes). Always `Some`.
fn table<'a>(parent: &'a mut Table, key: &str) -> Option<&'a mut Table> {
    let entry = parent
        .entry(key)
        .or_insert_with(|| Value::Table(Table::new()));
    if !entry.is_table() {
        *entry = Value::Table(Table::new());
    }
    entry.as_table_mut()
}

fn set(out: &mut Table, section: &str, key: &str, value: Value) {
    if let Some(t) = table(out, section) {
        t.insert(key.to_owned(), value);
    }
}

fn has(out: &Table, section: &str, key: &str) -> bool {
    out.get(section)
        .and_then(Value::as_table)
        .is_some_and(|t| t.contains_key(key))
}

fn push(out: &mut Table, section: &str, key: &str, value: Value) {
    let Some(section) = table(out, section) else {
        return;
    };
    match section.get_mut(key) {
        Some(Value::Array(items)) => items.push(value),
        _ => {
            section.insert(key.to_owned(), Value::Array(vec![value]));
        }
    }
}
