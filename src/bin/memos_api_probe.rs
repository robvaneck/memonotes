use anyhow::Result;
use memonotes::memos::MemosClient;

fn main() -> Result<()> {
    let client = MemosClient::from_env()?;
    if client.uses_plain_http() {
        eprintln!("Waarschuwing: HTTP versleutelt de PAT en notities niet tijdens transport.");
    }
    client.check_connection()?;
    let memos = client.list_memos()?;
    let spaces = client.list_spaces()?;
    println!(
        "Memos API werkt; {} actieve notities en {} spaces opgehaald.",
        memos.len(),
        spaces.len()
    );
    Ok(())
}
