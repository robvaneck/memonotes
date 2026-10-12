use crate::i18n::tr;
use anyhow::{Context, Result};
use gio::Cancellable;
use libsecret::{Schema, SchemaAttributeType, SchemaFlags};
use std::collections::HashMap;

const SCHEMA_NAME: &str = "app.memonotes.Account";

fn schema() -> Schema {
    let mut attributes = HashMap::new();
    attributes.insert("account_id", SchemaAttributeType::String);
    Schema::new(SCHEMA_NAME, SchemaFlags::NONE, attributes)
}

fn attributes(account_id: &str) -> HashMap<&'static str, &str> {
    HashMap::from([("account_id", account_id)])
}

pub fn store_token(account_id: &str, label: &str, token: &str) -> Result<()> {
    let schema = schema();
    libsecret::password_store_sync(
        Some(&schema),
        attributes(account_id),
        Some(libsecret::COLLECTION_DEFAULT),
        label,
        token,
        None::<&Cancellable>,
    )
    .context(tr("credentials.error.store_token_failed"))
}

pub fn lookup_token(account_id: &str) -> Result<Option<String>> {
    let schema = schema();
    Ok(
        libsecret::password_lookup_sync(
            Some(&schema),
            attributes(account_id),
            None::<&Cancellable>,
        )
        .context(tr("credentials.error.lookup_token_failed"))?
        .map(|token| token.to_string()),
    )
}

pub fn delete_token(account_id: &str) -> Result<()> {
    let schema = schema();
    libsecret::password_clear_sync(Some(&schema), attributes(account_id), None::<&Cancellable>)
        .context(tr("credentials.error.remove_token_failed"))?;
    Ok(())
}
