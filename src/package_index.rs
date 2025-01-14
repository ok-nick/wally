use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context};
use fs_err::{create_dir_all, File, OpenOptions};
use git2::Repository;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use url::Url;

use crate::git_util;
use crate::manifest::Manifest;
use crate::package_name::PackageName;

/// Configuration contained in the index's `config.json` file.
#[derive(Debug, Serialize, Deserialize)]
pub struct PackageIndexConfig {
    pub api: Url,
}

pub struct PackageIndex {
    /// URL of the remote index.
    url: Url,

    /// The path to the contents of the index, where we can retrieve packages.
    path: PathBuf,

    /// A Git repository handle that we can use to perform operations on the
    /// index repository, like updating or clearing it.
    repository: Mutex<Repository>,

    /// A cache that contains all of the packages we've queried so far. This
    /// cache is never emptied.
    package_cache: Mutex<HashMap<PackageName, Arc<PackageMetadata>>>,

    /// A GitHub Personal Access Token to use before trying the machine's local
    /// configuration.
    access_token: Option<String>,

    /// If this index is contained in a temporary location, like when running
    /// tests or a registry server, hold onto it here so that it'll be dropped
    /// at the right time.
    #[allow(unused)]
    temp_dir: Option<TempDir>,
}

impl PackageIndex {
    pub fn new(index_url: &Url, access_token: Option<String>) -> anyhow::Result<Self> {
        let path = index_path(index_url)?;
        let repository = git_util::open_or_clone(access_token.clone(), index_url, &path)?;

        let index = Self {
            url: index_url.clone(),
            path,
            repository: Mutex::new(repository),
            package_cache: Mutex::new(HashMap::new()),
            access_token,
            temp_dir: None,
        };

        index.update()?;
        Ok(index)
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn new_temp(index_url: &Url, access_token: Option<String>) -> anyhow::Result<Self> {
        let temp_dir = tempfile::tempdir()?;
        let path = temp_dir.path().to_owned();
        let repository = git_util::open_or_clone(access_token.clone(), index_url, &path)?;

        let index = Self {
            url: index_url.clone(),
            path,
            repository: Mutex::new(repository),
            package_cache: Mutex::new(HashMap::new()),
            access_token,
            temp_dir: Some(temp_dir),
        };

        index.update()?;
        Ok(index)
    }

    pub fn update(&self) -> anyhow::Result<()> {
        let repository = self.repository.lock().unwrap();

        log::info!("Updating package index...");
        git_util::update_index(self.access_token.clone(), &repository)
            .with_context(|| format!("could not update package index"))?;

        Ok(())
    }

    pub fn config(&self) -> anyhow::Result<PackageIndexConfig> {
        let config_path = self.path.join("config.json");
        let contents = fs_err::read_to_string(config_path)?;
        Ok(serde_json::from_str(&contents)?)
    }

    /// Publish a package to the local copy of the index and attempt to push it
    /// to the remote index, allowing a certain number of retries.
    ///
    /// Note that this method does not interact with any remote registry
    /// servers; it's intended for use with local registries or in the
    /// implementation of the registry server itself.
    pub fn publish(&self, manifest: &Manifest) -> anyhow::Result<()> {
        let repo = self.repository.lock().unwrap();

        let package_path = self.package_path(&manifest.package.name);

        // This package might not exist yet, so create its containing directory.
        create_dir_all(package_path.parent().unwrap())?;

        {
            let mut file = OpenOptions::new()
                .append(true)
                .create(true)
                .open(&package_path)?;

            // Package entries are newline-delimited JSON files. We assume here
            // that the file is empty or already ends in a newline.
            let mut entry = serde_json::to_string(&manifest)?;
            entry.push('\n');
            file.write_all(entry.as_bytes())?;
        }

        let message = format!("Publish {}", manifest.package_id());

        // libgit2 only accepts a relative path
        let relative_path = package_path.strip_prefix(&self.path).with_context(|| {
            format!(
                "Path {} was not relative to package path {}",
                package_path.display(),
                self.path.display()
            )
        })?;
        git_util::commit_and_push(&repo, self.access_token.clone(), &message, relative_path)?;
        // Blow away the cache for this package, since we've now modified the
        // underlying file.
        let mut package_cache = self.package_cache.lock().unwrap();
        package_cache.remove(&manifest.package.name);

        Ok(())
    }

    /// Read the list of versions for a package from the index.
    pub fn get_package_metadata(&self, name: &PackageName) -> anyhow::Result<Arc<PackageMetadata>> {
        let mut package_cache = self.package_cache.lock().unwrap();

        if package_cache.contains_key(name) {
            Ok(Arc::clone(&package_cache[name]))
        } else {
            let package_path = self.package_path(name);

            // Construct a buffered file reader, with a nice error message in the
            // event of failure. We might want to return a structured error from
            // this method in the future to distinguish between general I/O errors
            // and a package not existing.
            let file = File::open(&package_path)
                .with_context(|| format!("could not open package {} from index", name))?;
            let file = BufReader::new(file);

            // Read all of the manifests from the package file.
            //
            // Entries into the index are stored as JSON Lines. This block will
            // either parse all of the entries, or fail with a single error.
            let manifest_stream: Result<Vec<Manifest>, serde_json::Error> =
                serde_json::Deserializer::from_reader(file)
                    .into_iter::<Manifest>()
                    .collect();

            let versions = manifest_stream
                .with_context(|| format!("could not parse package index entry for {}", name))?;

            let metadata = Arc::new(PackageMetadata { versions });
            package_cache.insert(name.clone(), Arc::clone(&metadata));

            Ok(metadata)
        }
    }

    fn package_path(&self, name: &PackageName) -> PathBuf {
        // Each package has all of its versions stored in a folder based on its
        // scope and name.
        let mut package_path = self.path.clone();
        package_path.push(name.scope());
        package_path.push(name.name());
        package_path
    }
}

#[derive(Default)]
pub struct PackageMetadata {
    pub versions: Vec<Manifest>,
}

fn index_path(index_url: &Url) -> anyhow::Result<PathBuf> {
    let registry_name = match (index_url.domain(), index_url.scheme()) {
        (Some(domain), _) => domain,
        (None, "file") => "local-registry",
        _ => "unknown",
    };

    let hash = blake3::hash(index_url.to_string().as_bytes());
    let hash_hex = hex::encode(&hash.as_bytes()[..8]);
    let ident = format!("{}-{}", registry_name, hash_hex);

    let path = dirs::cache_dir()
        .ok_or_else(|| anyhow!("could not find cache directory"))?
        .join("wally")
        .join("index")
        .join(ident);

    Ok(path)
}
