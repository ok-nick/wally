//! Defines storage of authentication information when interacting with
//! registries.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use toml_edit::{value, Document};

const DEFAULT_AUTH_TOML: &str = r#"
# This is where Wally stores details for authenticating with regstries.
# It can be updated using `wally login` and `wally logout`.
"#;

#[derive(Serialize, Deserialize)]
pub struct AuthStore {
    pub token: Option<String>,
}

impl AuthStore {
    pub fn load() -> anyhow::Result<Self> {
        let path = file_path()?;
        let contents = Self::contents(&path)?;

        let auth = toml::from_str(&contents).with_context(|| {
            format!(
                "Malformed Wally auth config file. Try deleting {}",
                path.display()
            )
        })?;

        Ok(auth)
    }

    pub fn set_token(token: Option<&str>) -> anyhow::Result<()> {
        let path = file_path()?;
        let contents = Self::contents(&path)?;

        let mut auth: Document = contents.parse().unwrap();

        if let Some(token) = token {
            auth["token"] = value(token);
        } else {
            auth.as_table_mut().remove("token");
        }

        fs_err::create_dir_all(path.parent().unwrap())?;
        fs_err::write(&path, auth.to_string())?;

        Ok(())
    }

    fn contents(path: &Path) -> anyhow::Result<String> {
        match fs_err::read_to_string(&path) {
            Ok(contents) => Ok(contents),
            Err(err) => {
                if err.kind() == io::ErrorKind::NotFound {
                    Ok(DEFAULT_AUTH_TOML.to_owned())
                } else {
                    return Err(err.into());
                }
            }
        }
    }
}

fn file_path() -> anyhow::Result<PathBuf> {
    let mut path = dirs::home_dir().context("Failed to find home directory")?;
    path.push(".wally");
    path.push("auth.toml");
    Ok(path)
}
