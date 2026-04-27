//! `sam chat` — interactive CLI chat mode.
//!
//! Provides a terminal REPL that connects directly to the configured LLM
//! backend with full tool support (memory, twitter, claude_code, etc.).

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use tracing::{error, info, warn};

use sam_claude::{
    load_api_key, load_system_prompt, ConversationSession, LlmBackend, OpenAiCompatibleClient,
    SamClaudeClient, TokenBudget,
};
use sam_core::{config_path, load_config};
use sam_memory_adapter::MemoryAdapter;

pub async fn run() -> i32 {
    let config = match load_config(config_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return 2;
        }
    };

    // Load API key.
    let api_key = match load_api_key(&config.llm) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("API key error: {e}");
            return 2;
        }
    };

    let client: Arc<dyn LlmBackend> = match config.llm.provider.as_str() {
        "openai-compatible" => match OpenAiCompatibleClient::new(api_key, &config.llm) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                eprintln!("HTTP client error: {e}");
                return 2;
            }
        },
        _ => match SamClaudeClient::new(api_key, &config.llm) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                eprintln!("HTTP client error: {e}");
                return 2;
            }
        },
    };

    let system_prompt = load_system_prompt();
    let max_history = config.llm.max_history;
    let mut budget = TokenBudget::load_or_new(config.llm.daily_token_budget);

    // Long-term memory (optional).
    let mut memory: Option<MemoryAdapter> = match MemoryAdapter::from_config(&config.memory) {
        Ok(m) => {
            let stats = m.stats();
            info!(total_memories = stats.total_memories, "Memory system ready");
            Some(m)
        }
        Err(e) => {
            warn!("Memory system unavailable: {e}");
            None
        }
    };

    let mut session = ConversationSession::new("cli", system_prompt, max_history);

    // Print banner.
    println!("╔══════════════════════════════════════╗");
    println!("║        Sam — CLI Chat Mode           ║");
    println!("║  Provider: {:25}║", config.llm.provider);
    println!("║  Model:    {:25}║", config.llm.model);
    println!("║  Type 'exit' or Ctrl+D to quit       ║");
    println!("╚══════════════════════════════════════╝");
    println!();

    let stdin = io::stdin();
    let mut reader = stdin.lock();

    loop {
        // Print prompt.
        print!("You> ");
        io::stdout().flush().unwrap_or_default();

        // Read input.
        let mut input = String::new();
        match reader.read_line(&mut input) {
            Ok(0) => {
                // EOF (Ctrl+D).
                println!();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("input error: {e}");
                break;
            }
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input == "exit" || input == "quit" || input == "/quit" || input == "/exit" {
            break;
        }

        // Send to LLM.
        let reply = match session
            .reply(
                client.as_ref(),
                &mut budget,
                input,
                memory.as_mut(),
                &config,
                None,
            )
            .await
        {
            Ok(text) => text,
            Err(e) => {
                error!("LLM error: {e}");
                format!("Error: {e}")
            }
        };

        println!();
        println!("Sam> {reply}");
        println!();
    }

    println!("Bye!");
    0
}
