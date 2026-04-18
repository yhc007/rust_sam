//! `sam-agent import-memories` — bulk import memories from JSON file.

use tracing::{info, warn};

use sam_core::{config_path, load_config};
use sam_memory_adapter::MemoryAdapter;

#[derive(serde::Deserialize)]
struct MemoryEntry {
    text: String,
    #[serde(default)]
    tags: Vec<String>,
}

pub async fn run(file: String) -> i32 {
    let config = match load_config(config_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return 2;
        }
    };

    let mut memory = match MemoryAdapter::from_config(&config.memory) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Memory system error: {e}");
            return 2;
        }
    };

    let stats = memory.stats();
    info!(total_memories = stats.total_memories, "Memory system ready");

    let data = match std::fs::read_to_string(&file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Cannot read file '{file}': {e}");
            return 2;
        }
    };

    let entries: Vec<MemoryEntry> = match serde_json::from_str(&data) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Invalid JSON: {e}");
            return 2;
        }
    };

    println!("Importing {} memories...", entries.len());

    let mut ok = 0u32;
    let mut err = 0u32;
    for (i, entry) in entries.iter().enumerate() {
        match memory.store(&entry.text, entry.tags.clone()) {
            Ok(id) => {
                ok += 1;
                if (i + 1) % 10 == 0 || i + 1 == entries.len() {
                    println!("  [{}/{}] stored (last id: {id})", i + 1, entries.len());
                }
            }
            Err(e) => {
                err += 1;
                warn!(index = i, error = %e, "failed to store memory");
            }
        }
    }

    println!("\nDone: {ok} stored, {err} failed");
    if err > 0 { 1 } else { 0 }
}
