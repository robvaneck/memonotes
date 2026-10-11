use anyhow::Result;
use memonotes::adapters::{BackendAdapter, MemosAdapter};

fn main() -> Result<()> {
    let adapter = MemosAdapter::from_env()?;
    if adapter.uses_plain_http() {
        eprintln!("Waarschuwing: HTTP versleutelt de PAT en notities niet tijdens transport.");
    }
    adapter.check_connection()?;
    let memos = adapter.list_memos()?;
    let folders = adapter.list_folders()?;
    println!(
        "Memos API werkt; {} actieve notities en {} spaces opgehaald.",
        memos.len(),
        folders.len()
    );
    Ok(())
}
