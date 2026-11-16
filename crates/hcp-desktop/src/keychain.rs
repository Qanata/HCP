//! The dashboard's admin API key, kept in the macOS login Keychain.
//!
//! Keychain items are tied to the app that created them: another program running as you
//! (such as an agent's shell asking `security find-generic-password`) gets a prompt rather
//! than the key. That is the reason the key is not kept in a file.

use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};

const SERVICE: &str = "com.hcp.desktop";
const KEY: &str = "admin-api-key";
/// Not secret: the key's ID, so a replaced key can be revoked.
const KEY_ID: &str = "admin-api-key-id";

/// `errSecItemNotFound`.
const NOT_FOUND: i32 = -25300;

fn read(account: &str) -> Result<Option<String>, String> {
    match get_generic_password(SERVICE, account) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| "the Keychain item is not text".to_string()),
        Err(err) if err.code() == NOT_FOUND => Ok(None),
        Err(err) => Err(format!("cannot read the Keychain: {err}")),
    }
}

fn write(account: &str, value: &str) -> Result<(), String> {
    set_generic_password(SERVICE, account, value.as_bytes())
        .map_err(|err| format!("cannot write to the Keychain: {err}"))
}

fn remove(account: &str) -> Result<(), String> {
    match delete_generic_password(SERVICE, account) {
        Ok(()) => Ok(()),
        Err(err) if err.code() == NOT_FOUND => Ok(()),
        Err(err) => Err(format!("cannot update the Keychain: {err}")),
    }
}

pub fn key() -> Result<Option<String>, String> {
    read(KEY)
}

pub fn key_id() -> Result<Option<String>, String> {
    read(KEY_ID)
}

pub fn store(key: &str, key_id: Option<&str>) -> Result<(), String> {
    write(KEY, key)?;
    match key_id {
        Some(id) => write(KEY_ID, id),
        None => remove(KEY_ID),
    }
}

pub fn forget() -> Result<(), String> {
    remove(KEY)?;
    remove(KEY_ID)
}
