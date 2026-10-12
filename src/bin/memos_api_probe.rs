use anyhow::Result;
use memonotes::adapters::{BackendAdapter, MemosAdapter};
use memonotes::i18n::tr;

fn main() -> Result<()> {
    let adapter = MemosAdapter::from_env()?;
    if adapter.uses_plain_http() {
        eprintln!(
            "{}",
            tr("api.memos.warning.insecure_http_transport")
        );
    }
    adapter.check_connection()?;
    let memos = adapter.list_memos()?;
    let folders = adapter.list_folders()?;
    println!(
        "{}",
        memonotes::tr_format!(
            "api.memos.probe.success",
            notes = memos.len(),
            folders = folders.len()
        )
    );
    Ok(())
}
