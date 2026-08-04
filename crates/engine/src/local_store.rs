use std::path::{Path, PathBuf};

use comet_sync::DocsStore;

use crate::EngineError;

const LOCAL_STORE_DIR: &str = "local-store";

/// Create or open the one authoritative store for this Comet installation.
pub fn initialize_local_store(data_dir: &Path) -> Result<PathBuf, EngineError> {
    std::fs::create_dir_all(data_dir)?;
    let root = data_dir.join(LOCAL_STORE_DIR);
    DocsStore::open(&root)?;
    Ok(root)
}
