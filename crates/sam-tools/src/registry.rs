//! In-memory registry of discovered tool definitions.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use tracing::{debug, warn};
use walkdir::WalkDir;

use crate::schema::ToolDef;

/// A keyed collection of tool definitions, keyed by [`ToolDef::name`].
#[derive(Debug, Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolDef>,
}

impl ToolRegistry {
    /// Scan `dir` recursively for `*.toml` files and parse each into a
    /// [`ToolDef`]. Duplicate tool names cause an error.
    ///
    /// A missing `dir` is treated as an empty registry — useful when a user
    /// hasn't yet created `~/.sam/tools/`.
    pub fn scan(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let mut reg = Self::default();

        if !dir.exists() {
            debug!(path = %dir.display(), "tools dir missing; registry is empty");
            return Ok(reg);
        }

        for entry in WalkDir::new(dir).max_depth(4).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }

            match Self::load_file(path) {
                Ok(def) => reg.insert(def, path)?,
                Err(e) => {
                    warn!(file = %path.display(), error = %e, "skipping malformed tool file");
                }
            }
        }

        Ok(reg)
    }

    fn load_file(path: &Path) -> Result<ToolDef> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        ToolDef::from_toml(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    fn insert(&mut self, def: ToolDef, path: &Path) -> Result<()> {
        if self.tools.contains_key(&def.name) {
            bail!(
                "duplicate tool name `{}` (second definition in {})",
                def.name,
                path.display()
            );
        }
        self.tools.insert(def.name.clone(), def);
        Ok(())
    }

    /// Number of registered tools.
    pub fn count(&self) -> usize {
        self.tools.len()
    }

    /// Sorted list of tool names.
    pub fn list_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Lookup by name.
    pub fn get(&self, name: &str) -> Option<&ToolDef> {
        self.tools.get(name)
    }

    /// Iterate over `(name, def)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &ToolDef)> {
        self.tools.iter()
    }

    /// Build a registry from an in-memory iterator of tool defs; primarily
    /// useful in tests.
    pub fn from_iter_defs<I: IntoIterator<Item = ToolDef>>(
        iter: I,
        origin: &Path,
    ) -> Result<Self> {
        let mut reg = Self::default();
        for def in iter {
            reg.insert(def, origin)?;
        }
        Ok(reg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_tool(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(format!("{name}.toml"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    fn unique_tmp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("sam-tools-test-{tag}-{nanos}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn scans_multiple_tools() {
        let dir = unique_tmp("ok");
        write_tool(
            &dir,
            "hello",
            r#"
name = "hello"
description = "says hi"
tier = "chat"
[command]
program = "echo"
args = ["hi"]
"#,
        );
        write_tool(
            &dir,
            "list",
            r#"
name = "list"
description = "list files"
tier = "tier1"
[command]
program = "ls"
args = ["-la"]
[input_schema]
type = "object"
"#,
        );
        let reg = ToolRegistry::scan(&dir).expect("scan");
        assert_eq!(reg.count(), 2);
        let names = reg.list_names();
        assert!(names.contains(&"hello".to_string()));
        assert!(names.contains(&"list".to_string()));
        let list = reg.get("list").unwrap();
        assert!(list.input_schema_raw.contains("object"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn duplicate_names_err() {
        let dir = unique_tmp("dup");
        write_tool(
            &dir,
            "a",
            r#"
name = "dup"
description = "first"
tier = "chat"
[command]
program = "true"
"#,
        );
        write_tool(
            &dir,
            "b",
            r#"
name = "dup"
description = "second"
tier = "chat"
[command]
program = "false"
"#,
        );
        let err = ToolRegistry::scan(&dir).unwrap_err();
        assert!(
            err.to_string().contains("duplicate tool name"),
            "unexpected err: {err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_dir_is_empty() {
        let missing = PathBuf::from("/nonexistent/sam-tools-test-dir-xyz");
        let reg = ToolRegistry::scan(&missing).expect("scan missing");
        assert_eq!(reg.count(), 0);
    }

    #[test]
    fn malformed_file_is_skipped() {
        let dir = unique_tmp("bad");
        write_tool(&dir, "broken", "this = is = not = toml");
        write_tool(
            &dir,
            "ok",
            r#"
name = "ok"
description = "good"
tier = "chat"
[command]
program = "true"
"#,
        );
        let reg = ToolRegistry::scan(&dir).expect("scan");
        assert_eq!(reg.count(), 1);
        assert!(reg.get("ok").is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
