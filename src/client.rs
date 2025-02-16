use std::{str::FromStr, sync::Arc};

use crate::{
    error::*, AnnouncementKey, ArtifactSet, ArtifactSetId, Owner, PackageName, Release, ReleaseKey,
    ReleaseList, ReleaseTag, SourceHost, UnparsedUrl, UnparsedVersion,
};
use axoasset::LocalAsset;
use camino::Utf8PathBuf;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, Url,
};
use serde::{Deserialize, Serialize};

/// A domain (as in part of a URL)
type Domain = String;

/// A client for The Abyss
///
/// This type intentionally does not implement Debug, to avoid leaking authentication secrets.
#[derive(Clone)]
pub struct Gazenot(Arc<GazenotInner>);

#[doc(hidden)]
/// Implementation detail of Gazenot
///
/// DO NOT IMPLEMENT DEBUG ON THIS TYPE, IT CONTAINS SECRET API KEYS AT RUNTIME
pub struct GazenotInner {
    /// Domain for the main abyss API
    api_server: Domain,
    /// Domain where ArtifactSet downloads are GETtable from
    hosting_server: Domain,
    /// Auth for requests
    auth_headers: HeaderMap,
    /// Owner of the project
    owner: Owner,
    /// Name of the project
    source_host: SourceHost,
    /// reqwest client
    client: Client,
}

impl std::ops::Deref for Gazenot {
    type Target = GazenotInner;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Deserialize, Debug, Clone)]
struct Response<T> {
    success: bool,
    result: Option<T>,
    errors: Option<Vec<String>>,
}

#[derive(Deserialize, Debug, Clone)]
struct BasicResponse {
    success: bool,
    errors: Option<Vec<String>>,
}

#[derive(Deserialize, Debug, Clone)]
struct ArtifactSetResponse {
    public_id: ArtifactSetId,
    set_download_url: Option<UnparsedUrl>,
    upload_url: Option<UnparsedUrl>,
    release_url: Option<UnparsedUrl>,
    announce_url: Option<UnparsedUrl>,
}

#[derive(Serialize, Debug, Clone)]
struct CreateReleaseRequest {
    release: CreateReleaseRequestInner,
}

#[derive(Serialize, Debug, Clone)]
struct CreateReleaseRequestInner {
    artifact_set_id: String,
    tag: ReleaseTag,
    version: UnparsedVersion,
    is_prerelease: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ReleaseResponse {
    release_download_url: Option<UnparsedUrl>,
}

#[derive(Serialize, Debug, Clone)]
struct AnnounceReleaseKey {
    package: PackageName,
    tag: ReleaseTag,
}

#[derive(Serialize, Debug, Clone)]
struct AnnounceReleaseRequest {
    releases: Vec<AnnounceReleaseKey>,
    body: String,
}

#[derive(Deserialize, Debug, Clone)]
struct ListReleasesResponse {
    // TBD
}

impl Gazenot {
    /// Gaze Not Into The Abyss, Lest You Become A Release Engineer
    ///
    /// This is the vastly superior alias for [`Gazenot::new`].
    pub fn into_the_abyss(
        source_host: impl Into<SourceHost>,
        owner: impl Into<Owner>,
    ) -> Result<Self> {
        Self::new(source_host, owner)
    }

    /// Create a new authenticated client for The Abyss
    ///
    /// Authentication requires an Axo Releases Token, whose value
    /// is currently sourced from an AXO_RELEASES_TOKEN environment variable.
    /// It's an error for that variable to not be properly set.
    ///
    /// This is the vastly inferior alias for [`Gazenot::into_the_abyss`].
    ///
    /// See also, `[Abyss::new_unauthed][]`.
    pub fn new(source_host: impl Into<SourceHost>, owner: impl Into<Owner>) -> Result<Self> {
        let source_host = source_host.into();
        let owner = owner.into();

        let auth_headers = auth_headers(&source_host, &owner)
            .map_err(|e| GazenotError::new("initializing Abyss authentication", e))?;

        Self::new_with_auth_headers(source_host, owner, auth_headers)
    }

    /// Create a new client for The Abyss with no authentication
    ///
    /// This creates a client that is only suitable for accessing certain kinds of endpoint, such as:
    ///
    /// * [`Gazenot::list_releases_many``][]
    /// * [`Gazenot::download_artifact_set_url``][]
    pub fn new_unauthed(
        source_host: impl Into<SourceHost>,
        owner: impl Into<Owner>,
    ) -> Result<Self> {
        let auth_headers = HeaderMap::new();

        Self::new_with_auth_headers(source_host.into(), owner.into(), auth_headers)
    }

    fn new_with_auth_headers(
        source_host: SourceHost,
        owner: Owner,
        auth_headers: HeaderMap,
    ) -> Result<Self> {
        const DESC: &str = "create http client for axodotdev hosting (abyss)";
        const API_SERVER: &str = "axo-abyss.fly.dev";
        const HOSTING_SERVER: &str = "artifacts.axodotdev.host";

        let timeout = std::time::Duration::from_secs(10);
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| GazenotError::new(DESC, e))?;

        Ok(Self(Arc::new(GazenotInner {
            api_server: API_SERVER.to_owned(),
            hosting_server: HOSTING_SERVER.to_owned(),
            owner,
            source_host,
            auth_headers,
            client,
        })))
    }

    /// Ask The Abyss to create new ArtifactSets for the given packages
    pub async fn create_artifact_sets(
        &self,
        packages: impl IntoIterator<Item = PackageName>,
    ) -> Result<Vec<ArtifactSet>> {
        // Spawn all the queries in parallel...
        let mut queries = Vec::new();
        for package in packages {
            // Abyss is just an Arc wrapper around the real client, so Cloning is fine
            let handle = self.clone();
            let desc = format!(
                "create hosting for {}/{}/{}",
                self.source_host, self.owner, package
            );
            let url = self
                .create_artifact_set_url(&package)
                .map_err(|e| GazenotError::new(&desc, e))?;
            queries.push((
                desc,
                url.clone(),
                tokio::spawn(async move { handle.create_artifact_set(url, package).await }),
            ));
        }

        // Then join on them all
        join_all(queries).await
    }

    /// Ask The Abyss to create a new ArtifactSets for the given package
    async fn create_artifact_set(
        &self,
        url: Url,
        package: PackageName,
    ) -> ResultInner<ArtifactSet> {
        // No body
        let response = self
            .client
            .post(url.clone())
            .headers(self.auth_headers.clone())
            .send()
            .await?;

        // Process the response
        let ArtifactSetResponse {
            public_id,
            set_download_url,
            upload_url,
            release_url,
            announce_url,
        } = process_response(response).await?;

        // Add extra context to make the response more useful in code
        Ok(ArtifactSet {
            package,
            public_id,
            set_download_url,
            upload_url,
            release_url,
            announce_url,
        })
    }

    /// Upload files to several ArtifactSets
    ///
    /// The input is a list of files to upload, but with each file parented
    /// to the ArtifactSet it should be uploaded to.
    ///
    /// This is a bit of an awkward signature, but it lets us handle all the parallelism for you!
    pub async fn upload_files(
        &self,
        files: impl IntoIterator<Item = (&ArtifactSet, Vec<Utf8PathBuf>)>,
    ) -> Result<()> {
        // Spawn all the queries in parallel...
        let mut queries = vec![];
        for (set, sub_files) in files {
            for file in sub_files {
                let handle = self.clone();
                let filename = file.file_name().unwrap();
                let desc = format!(
                    "upload {filename} to hosting for {}/{}/{}",
                    self.source_host, self.owner, set.package
                );
                reject_mock(set).map_err(|e| GazenotError::new(&desc, e))?;
                let url = self
                    .upload_artifact_set_url(set, filename)
                    .map_err(|e| GazenotError::new(&desc, e))?;
                queries.push((
                    desc,
                    url.clone(),
                    tokio::spawn(async move { handle.upload_file(url, file).await }),
                ));
            }
        }

        // Then join on them all
        join_all(queries).await?;

        Ok(())
    }

    /// Single file portion of upload_file
    ///
    /// Not exposed as a public because you shouldn't use this directly,
    /// and we might want to rework it.
    async fn upload_file(&self, url: Url, path: Utf8PathBuf) -> ResultInner<()> {
        // Load the bytes from disk
        //
        // FIXME: this should be streamed to the request as it's loaded to disk
        let data = LocalAsset::load(path)?;

        // Send the bytes
        let response = self
            .client
            .post(url.clone())
            .headers(self.auth_headers.clone())
            .header("content-type", "application/octet-stream")
            .body(data.contents)
            .send()
            .await?;

        process_response_basic(response).await?;

        Ok(())
    }

    /// Create Releases for all the given ArtifactSets
    pub async fn create_releases(
        &self,
        releases: impl IntoIterator<Item = (&ArtifactSet, ReleaseKey)>,
    ) -> Result<Vec<Release>> {
        // Spawn all the queries in parallel...
        let mut queries = Vec::new();
        for (set, key) in releases {
            // Abyss is just an Arc wrapper around the real client, so Cloning is fine
            let handle = self.clone();
            let package = set.package.clone();
            let announce_url = set.announce_url.clone();
            let set_id = set.public_id.clone();
            let desc = format!(
                "create release for {}/{}/{}",
                self.source_host, self.owner, set.package
            );
            reject_mock(set).map_err(|e| GazenotError::new(&desc, e))?;
            let url = self
                .create_release_url(set)
                .map_err(|e| GazenotError::new(&desc, e))?;
            queries.push((
                desc,
                url.clone(),
                tokio::spawn(async move {
                    handle
                        .create_release(url, set_id, package, announce_url, key)
                        .await
                }),
            ));
        }

        // Then join on them all
        join_all(queries).await
    }

    async fn create_release(
        &self,
        url: Url,
        set_id: ArtifactSetId,
        package: PackageName,
        announce_url: Option<UnparsedUrl>,
        release: ReleaseKey,
    ) -> ResultInner<Release> {
        let request = CreateReleaseRequest {
            release: CreateReleaseRequestInner {
                artifact_set_id: set_id,
                tag: release.tag.clone(),
                version: release.version.clone(),
                is_prerelease: release.is_prerelease,
            },
        };

        let response = self
            .client
            .post(url.clone())
            .headers(self.auth_headers.clone())
            .json(&request)
            .send()
            .await?;

        // Parse the result
        let ReleaseResponse {
            release_download_url,
        } = process_response(response).await?;
        Ok(Release {
            package,
            tag: release.tag,
            release_download_url,
            announce_url,
        })
    }

    pub async fn create_announcements(
        &self,
        releases: impl IntoIterator<Item = &Release>,
        announcement: AnnouncementKey,
    ) -> Result<()> {
        // Sort the releases by owner (this should always select one owner, but hey why not...)
        let releases = releases.into_iter().collect::<Vec<_>>();
        let Some(some_release) = releases.first() else {
            return Ok(());
        };
        let desc = format!(
            "create announcement for {}/{}/{}",
            self.source_host, self.owner, some_release.tag
        );
        let url = self
            .create_announcement_url(some_release)
            .map_err(|e| GazenotError::new(&desc, e))?;

        // Spawn all the queries in parallel... (there's only one lol)
        let mut queries = Vec::new();
        {
            let handle = self.clone();
            let releases = releases
                .iter()
                .map(|r| AnnounceReleaseKey {
                    package: r.package.clone(),
                    tag: r.tag.clone(),
                })
                .collect();
            let announcement = announcement.clone();
            queries.push((
                desc,
                url.clone(),
                tokio::spawn(async move {
                    handle
                        .create_announcement(url, releases, announcement)
                        .await
                }),
            ));
        }

        // Then join on them all
        join_all(queries).await?;
        Ok(())
    }

    async fn create_announcement(
        &self,
        url: Url,
        releases: Vec<AnnounceReleaseKey>,
        announcement: AnnouncementKey,
    ) -> ResultInner<()> {
        let request = AnnounceReleaseRequest {
            releases,
            body: announcement.body,
        };
        let response = self
            .client
            .post(url.clone())
            .headers(self.auth_headers.clone())
            .json(&request)
            .send()
            .await?;

        process_response_basic(response).await
    }

    /// Ask The Abyss about releases for several packages
    pub async fn list_releases_many(
        &self,
        packages: impl IntoIterator<Item = PackageName>,
    ) -> Result<Vec<ReleaseList>> {
        // Spawn all the queries in parallel...
        let mut queries = Vec::new();
        for package in packages {
            // Abyss is just an Arc wrapper around the real client, so Cloning is fine
            let handle = self.clone();
            let desc = format!(
                "get releases for {}/{}/{}",
                self.source_host, self.owner, package
            );
            let url = self
                .list_releases_url(&package)
                .map_err(|e| GazenotError::new(&desc, e))?;
            queries.push((
                desc,
                url.clone(),
                tokio::spawn(async move { handle.list_releases(url, package).await }),
            ));
        }

        // Then join on them all
        join_all(queries).await
    }

    /// Ask The Abyss about releases
    async fn list_releases(&self, url: Url, package: PackageName) -> ResultInner<ReleaseList> {
        // No body
        let response = self
            .client
            .get(url.clone())
            .headers(self.auth_headers.clone())
            .send()
            .await?;

        // Process the response
        let ListReleasesResponse {} = process_response(response).await?;

        // Add extra context to make the response more useful in code
        Ok(ReleaseList { package })
    }

    pub fn create_artifact_set_url(&self, package: &PackageName) -> ResultInner<Url> {
        // POST /:sourcehost/:owner/:package/artifacts
        let server = &self.api_server;
        let source_host = &self.source_host;
        let owner = &self.owner;
        let url = Url::from_str(&format!(
            "https://{server}/{source_host}/{owner}/{package}/artifacts"
        ))?;
        Ok(url)
    }

    pub fn download_artifact_set_url(&self, set: &ArtifactSet, filename: &str) -> ResultInner<Url> {
        // TODO: update this to new signature
        // GET :owner.:hosting_server/:package/:public_id/
        let base = set.set_download_url.clone().unwrap_or_else(|| {
            let server = &self.hosting_server;
            let owner = &self.owner;
            let ArtifactSet {
                package, public_id, ..
            } = set;
            format!("https://{owner}.{server}/{package}/{public_id}")
        });
        let url = Url::from_str(&format!("{base}/{filename}"))?;
        Ok(url)
    }

    pub fn upload_artifact_set_url(&self, set: &ArtifactSet, filename: &str) -> ResultInner<Url> {
        // POST /:sourcehost/:owner/:package/artifacts/:id/
        let base = set.upload_url.clone().unwrap_or_else(|| {
            let server = &self.api_server;
            let source_host = &self.source_host;
            let owner = &self.owner;
            let ArtifactSet {
                package, public_id, ..
            } = set;
            format!("https://{server}/{source_host}/{owner}/{package}/artifacts/{public_id}/upload")
        });
        let url = Url::from_str(&format!("{base}/{filename}"))?;
        Ok(url)
    }

    pub fn create_release_url(&self, set: &ArtifactSet) -> ResultInner<Url> {
        // POST /:sourcehost/:owner/:package/releases
        let url = set.release_url.clone().unwrap_or_else(|| {
            let server = &self.api_server;
            let source_host = &self.source_host;
            let owner = &self.owner;
            let package = &set.package;
            format!("https://{server}/{source_host}/{owner}/{package}/releases")
        });
        let url = Url::from_str(&url)?;
        Ok(url)
    }

    pub fn create_announcement_url(&self, release: &Release) -> ResultInner<Url> {
        // POST /:sourcehost/:owner/announcements
        let url = release.announce_url.clone().unwrap_or_else(|| {
            let server = &self.api_server;
            let source_host = &self.source_host;
            let owner = &self.owner;
            format!("https://{server}/{source_host}/{owner}/announcements")
        });
        let url = Url::from_str(&url)?;
        Ok(url)
    }

    pub fn list_releases_url(&self, package: &PackageName) -> ResultInner<Url> {
        // GET /:sourcehost/:owner/:projects/releases
        let server = &self.api_server;
        let source_host = &self.source_host;
        let owner = &self.owner;
        let package = &package;
        let url = Url::from_str(&format!(
            "https://{server}/{source_host}{owner}/{package}/releases"
        ))?;
        Ok(url)
    }
}

async fn join_all<T>(
    queries: impl IntoIterator<Item = (String, Url, tokio::task::JoinHandle<ResultInner<T>>)>,
) -> Result<Vec<T>> {
    let mut results = Vec::new();
    for (desc, url, query) in queries {
        let result = query
            .await
            .map_err(|e| GazenotError::with_url(&desc, url.to_string(), e))?
            .map_err(|e| GazenotError::with_url(&desc, url.to_string(), e))?;
        results.push(result);
    }
    Ok(results)
}

fn auth_headers(source: &SourceHost, owner: &Owner) -> ResultInner<HeaderMap> {
    // extra-awkard code so you're on your toes and properly treat this like radioactive waste
    // DO NOT UNDER ANY CIRCUMSTANCES PRINT THIS VALUE.
    // DO NOT IMPLEMENT DEBUG ON Abyss OR AbyssInner!!
    let auth = {
        // Intentionally hidden so we only do this here
        const AUTH_KEY_ENV_VAR: &str = "AXO_RELEASES_TOKEN";
        // Load from env-var
        let Ok(auth_key) = std::env::var(AUTH_KEY_ENV_VAR) else {
            return Err(GazenotErrorInner::AuthKey {
                reason: "could not load env var",
                env_var_name: AUTH_KEY_ENV_VAR,
            });
        };
        if auth_key.is_empty() {
            return Err(GazenotErrorInner::AuthKey {
                reason: "no value in env var",
                env_var_name: AUTH_KEY_ENV_VAR,
            });
        }
        // Create http header
        let Ok(auth) = HeaderValue::from_str(&format!("Bearer {auth_key}")) else {
            return Err(GazenotErrorInner::AuthKey {
                reason: "had invalid characters for an http header",
                env_var_name: AUTH_KEY_ENV_VAR,
            });
        };
        auth
    };

    let id = HeaderValue::from_str(&format!("{source}/{owner}"))?;
    let auth_headers = HeaderMap::from_iter([
        (HeaderName::from_static("authorization"), auth),
        (HeaderName::from_static("x-axo-identifier"), id),
    ]);
    Ok(auth_headers)
}

async fn process_response<T: for<'a> Deserialize<'a>>(
    response: reqwest::Response,
) -> ResultInner<T> {
    // don't use status_for_error, we want to try to parse errors!
    let status = response.status();

    // Load the text of the response
    let text = response.text().await?;

    // Try to parse the response as json
    let Ok(parsed): std::result::Result<Response<T>, _> = axoasset::serde_json::de::from_str(&text)
    else {
        // Failed to parse response as json, error out and display whatever text as an error
        let errors = if text.is_empty() {
            vec![]
        } else {
            vec![SimpleError(text.clone())]
        };
        return Err(GazenotErrorInner::ResponseError { status, errors });
    };

    // Only return success if everything agrees
    if parsed.success && status.is_success() {
        if let Some(result) = parsed.result {
            return Ok(result);
        }
    }

    // Otherwise return an error

    // Add extra context if the server is sending us gibberish
    let has_cohesion =
        parsed.success == status.is_success() && parsed.success == parsed.result.is_some();
    let extra_error = if !has_cohesion {
        Some(format!("server response inconsistently reported success -- status: {}, .success: {}, .result.is_some(): {}", status, parsed.success, parsed.result.is_some()))
    } else {
        None
    };

    Err(GazenotErrorInner::ResponseError {
        status,
        errors: parsed
            .errors
            .unwrap_or_default()
            .into_iter()
            .chain(extra_error)
            .map(SimpleError)
            .collect(),
    })
}

async fn process_response_basic(response: reqwest::Response) -> ResultInner<()> {
    // don't use status_for_error, we want to try to parse errors!
    let status = response.status();

    // Load the text of the response
    let text = response.text().await?;

    // Try to parse the response as json
    let Ok(parsed): std::result::Result<BasicResponse, _> =
        axoasset::serde_json::de::from_str(&text)
    else {
        // Failed to parse response as json, error out and display whatever text as an error
        let errors = if text.is_empty() {
            vec![]
        } else {
            vec![SimpleError(text.clone())]
        };
        return Err(GazenotErrorInner::ResponseError { status, errors });
    };

    // Only return success if everything agrees
    if parsed.success && status.is_success() {
        return Ok(());
    }

    // Otherwise return an error

    // Add extra context if the server is sending us gibberish
    let has_cohesion = parsed.success == status.is_success();
    let extra_error = if !has_cohesion {
        Some(format!(
            "server response inconsistently reported success -- status: {}, .success: {}",
            status, parsed.success
        ))
    } else {
        None
    };

    Err(GazenotErrorInner::ResponseError {
        status,
        errors: parsed
            .errors
            .unwrap_or_default()
            .into_iter()
            .chain(extra_error)
            .map(SimpleError)
            .collect(),
    })
}

fn reject_mock(artifact_set: &ArtifactSet) -> ResultInner<()> {
    if artifact_set.is_mock() {
        Err(GazenotErrorInner::IsMocked)
    } else {
        Ok(())
    }
}
