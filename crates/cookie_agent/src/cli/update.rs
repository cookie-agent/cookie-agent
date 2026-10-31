//! `cookie update` (alias `cookie upgrade`): replaces this binary with the
//! newest release on the channel it was installed from
//! (`docs/site/specs/self-update.md`).

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, anyhow, bail};
use cookie_agent_engine::release::{self, Channel, ReleaseVersion};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The GitHub API of the repository releases are published from.
pub(crate) const RELEASES_API: &str = "https://api.github.com/repos/cookie-agent/cookie-agent";

/// A GitHub token sent with API requests when set, as the installers read
/// it.
const TOKEN_ENV: &str = "COOKIE_AGENT_GITHUB_TOKEN";

/// How long the downloaded binary may take to print its version.
const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How many releases the nightly lookup reads, newest first: more than the
/// nightlies pruning keeps.
const NIGHTLY_PAGE: usize = 30;

#[derive(Debug, PartialEq, Eq, clap::Args)]
pub(crate) struct UpdateArgs {
    /// Report the installed and the newest release, and change nothing.
    #[arg(long)]
    pub(crate) check: bool,
    /// Follow this channel instead of the installed build's, switching to
    /// it. Required to replace a build from source.
    #[arg(long, value_enum, conflicts_with = "version")]
    pub(crate) channel: Option<ChannelArg>,
    /// Install exactly this release, such as `v0.3.0`, older ones included.
    #[arg(long, value_name = "TAG")]
    pub(crate) version: Option<String>,
    /// Reinstall the newest release even when it is the installed one.
    #[arg(long)]
    pub(crate) force: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ChannelArg {
    Stable,
    Nightly,
}

impl From<ChannelArg> for Channel {
    fn from(channel: ChannelArg) -> Self {
        match channel {
            ChannelArg::Stable => Self::Stable,
            ChannelArg::Nightly => Self::Nightly,
        }
    }
}

/// The build being updated.
#[derive(Clone, Debug)]
pub(crate) struct Installed {
    /// Its release tag; empty for a build from source.
    pub(crate) tag: String,
    /// Its version: the tag without the `v`, else the package version.
    pub(crate) version: String,
    pub(crate) channel: Channel,
    /// The target triple its release archive is named for.
    pub(crate) target: String,
}

impl Installed {
    fn this_build() -> Self {
        Self {
            tag: release::RELEASE_TAG.to_owned(),
            version: release::version().to_owned(),
            channel: release::channel(),
            target: release::TARGET.to_owned(),
        }
    }

    fn describe(&self) -> String {
        format!("{} ({})", self.version, self.channel)
    }
}

/// What the user asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Wanted {
    Channel(Channel),
    Tag(String),
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// A release that has this target's archive and its checksum.
#[derive(Debug)]
struct Candidate<'a> {
    tag: &'a str,
    version: ReleaseVersion,
    archive: &'a Asset,
    checksum: &'a Asset,
}

/// `cookie update [--check] [--channel ...] [--version <tag>] [--force]`.
pub(crate) async fn run_update(args: UpdateArgs) -> anyhow::Result<()> {
    let installed = Installed::this_build();
    let wanted = wanted(&installed, &args)?;
    let releases = Releases::new(RELEASES_API, std::env::var(TOKEN_ENV).ok())?;
    let executable = if args.check {
        None
    } else {
        Some(running_executable()?)
    };
    let install = executable.as_deref().map(|executable| InstallAt {
        executable,
        receipt: receipt_path(),
    });
    let Some(updated) = update(&releases, &installed, &wanted, &args, install).await? else {
        return Ok(());
    };
    report_daemon(&updated).await;
    Ok(())
}

/// The channel or tag `args` ask for, refusing a build from source unless
/// they name one.
fn wanted(installed: &Installed, args: &UpdateArgs) -> anyhow::Result<Wanted> {
    if let Some(tag) = &args.version {
        let tag = if tag.starts_with('v') {
            tag.clone()
        } else {
            format!("v{tag}")
        };
        if ReleaseVersion::parse(&tag).is_none() {
            bail!("`{tag}` is not a release tag such as v0.4.0 or v0.4.0-alpha.1793047587");
        }
        return Ok(Wanted::Tag(tag));
    }
    match (args.channel, installed.channel) {
        (Some(channel), _) => Ok(Wanted::Channel(channel.into())),
        (None, Channel::Source) => bail!(
            "cookie {} was built from source, so `cookie update` does not know which release \
             to track. Rebuild it the way you built it, or replace it with a release build:\n  \
             cookie update --channel stable\n  cookie update --channel nightly",
            installed.version
        ),
        (None, channel) => Ok(Wanted::Channel(channel)),
    }
}

/// Where an update installs: the binary to replace and the installers'
/// receipt to keep in step with it.
struct InstallAt<'a> {
    executable: &'a Path,
    receipt: Option<PathBuf>,
}

/// Finds the release `wanted` names and, unless `--check` or it is already
/// installed, installs it at `at`. Returns the installed version when the
/// binary was replaced.
async fn update(
    releases: &Releases,
    installed: &Installed,
    wanted: &Wanted,
    args: &UpdateArgs,
    at: Option<InstallAt<'_>>,
) -> anyhow::Result<Option<String>> {
    let found = releases.find(wanted, &installed.target).await?;
    let candidate = candidate(&found, &installed.target)?;
    let to = describe_tag(candidate.tag);
    let newer = is_newer(installed, wanted, &candidate);
    if args.check {
        if newer {
            println!("update available: {} → {to}", installed.describe());
        } else {
            println!("up to date: {}", installed.describe());
        }
        return Ok(None);
    }
    if !newer && !args.force {
        println!("up to date: {}", installed.describe());
        return Ok(None);
    }
    let InstallAt {
        executable,
        receipt,
    } = at.context("no executable to replace")?;
    if is_downgrade(installed, &candidate.version) {
        println!(
            "note: {to} is older than {}; it may report configuration or session history \
             it does not know",
            installed.version
        );
    }
    println!("cookie {} → {to}", installed.describe());
    install(releases, &candidate, executable).await?;
    println!("replaced {}", executable.display());
    let version = release_version_text(candidate.tag);
    if let Some(receipt) = receipt
        && let Err(error) = update_receipt(&receipt, executable, &version)
    {
        println!(
            "note: could not update the install receipt {}: {error}",
            receipt.display()
        );
    }
    Ok(Some(version))
}

/// `wanted`'s release among `found`: the one release a channel's or tag's
/// lookup returned, or the newest nightly with this target's archive.
fn candidate<'a>(found: &'a Found, target: &str) -> anyhow::Result<Candidate<'a>> {
    match found {
        Found::One(release) => installable(release, target).ok_or_else(|| {
            if has_any_archive(release) {
                anyhow!(
                    "{} has no archive for {target}; install another release or build from \
                     source",
                    release.tag_name
                )
            } else {
                anyhow!(
                    "{} is still being uploaded; try again in a few minutes",
                    release.tag_name
                )
            }
        }),
        Found::Nightlies(releases) => newest_nightly(releases, target)
            .ok_or_else(|| anyhow!("no nightly release has an archive for {target}")),
    }
}

/// Whether `candidate` is newer than `installed` on `wanted`'s terms: a tag
/// or another channel is always installed unless it is the installed tag;
/// on the installed channel, stables go by version and nightlies by stamp.
fn is_newer(installed: &Installed, wanted: &Wanted, candidate: &Candidate<'_>) -> bool {
    if candidate.tag == installed.tag {
        return false;
    }
    let installed_version = ReleaseVersion::parse(&installed.tag);
    match (wanted, installed_version) {
        (Wanted::Channel(channel), Some(current)) if *channel == installed.channel => match channel
        {
            Channel::Stable => candidate.version.core > current.core,
            Channel::Nightly => {
                candidate.version.nightly_stamp().unwrap_or(0)
                    > current.nightly_stamp().unwrap_or(0)
            }
            Channel::Source => false,
        },
        _ => true,
    }
}

/// Whether installing `target` goes back to an older major.minor.patch.
fn is_downgrade(installed: &Installed, target: &ReleaseVersion) -> bool {
    ReleaseVersion::parse(&installed.version).is_some_and(|current| target.core < current.core)
}

/// The newest nightly in `releases` with this target's archive, by its
/// stamp: a nightly still uploading is passed over for the one before.
fn newest_nightly<'a>(releases: &'a [Release], target: &str) -> Option<Candidate<'a>> {
    releases
        .iter()
        .filter(|release| release.prerelease)
        .filter_map(|release| installable(release, target))
        .filter(|candidate| candidate.version.nightly_stamp().is_some())
        .max_by_key(|candidate| candidate.version.nightly_stamp())
}

/// `release` as a candidate, when it is published and has this target's
/// archive and checksum.
fn installable<'a>(release: &'a Release, target: &str) -> Option<Candidate<'a>> {
    if release.draft {
        return None;
    }
    let version = ReleaseVersion::parse(&release.tag_name)?;
    let name = archive_name(target);
    let checksum_name = format!("{name}.sha256");
    let archive = release.assets.iter().find(|asset| asset.name == name)?;
    let checksum = release
        .assets
        .iter()
        .find(|asset| asset.name == checksum_name)?;
    Some(Candidate {
        tag: &release.tag_name,
        version,
        archive,
        checksum,
    })
}

fn has_any_archive(release: &Release) -> bool {
    release.assets.iter().any(|asset| {
        asset.name.starts_with("cookie_agent-")
            && (asset.name.ends_with(".tar.xz") || asset.name.ends_with(".zip"))
    })
}

/// The release archive cargo-dist names for `target`.
fn archive_name(target: &str) -> String {
    let extension = if target.contains("windows") {
        "zip"
    } else {
        "tar.xz"
    };
    format!("cookie_agent-{target}.{extension}")
}

fn release_version_text(tag: &str) -> String {
    tag.strip_prefix('v').unwrap_or(tag).to_owned()
}

fn describe_tag(tag: &str) -> String {
    format!(
        "{} ({})",
        release_version_text(tag),
        release::channel_of(tag)
    )
}

/// What a lookup returned.
enum Found {
    /// The release a tag or the stable channel names.
    One(Release),
    /// The newest releases, nightlies among them.
    Nightlies(Vec<Release>),
}

/// The GitHub releases API.
pub(crate) struct Releases {
    client: reqwest::Client,
    api: String,
    token: Option<String>,
}

impl Releases {
    pub(crate) fn new(api: &str, token: Option<String>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("cookie-agent/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .build()
            .context("start the HTTP client")?;
        Ok(Self {
            client,
            api: api.trim_end_matches('/').to_owned(),
            token: token.filter(|token| !token.is_empty()),
        })
    }

    async fn find(&self, wanted: &Wanted, target: &str) -> anyhow::Result<Found> {
        Ok(match wanted {
            Wanted::Tag(tag) => Found::One(
                self.get(&format!("/releases/tags/{tag}"))
                    .await?
                    .ok_or_else(|| anyhow!("no release is tagged {tag}"))?,
            ),
            Wanted::Channel(Channel::Stable) => Found::One(
                self.get("/releases/latest")
                    .await?
                    .ok_or_else(|| anyhow!("no stable release is published yet"))?,
            ),
            Wanted::Channel(Channel::Nightly | Channel::Source) => Found::Nightlies(
                self.get(&format!("/releases?per_page={NIGHTLY_PAGE}"))
                    .await?
                    .ok_or_else(|| anyhow!("no nightly release has an archive for {target}"))?,
            ),
        })
    }

    /// GETs `path` under the API as JSON; `None` for a 404.
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> anyhow::Result<Option<T>> {
        let url = format!("{}{path}", self.api);
        let mut request = self
            .client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("look up releases at {url}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            bail!(
                "GitHub refused the release lookup ({status}), most likely its rate limit for \
                 unauthenticated requests; try again later or set {TOKEN_ENV} to a GitHub token"
            );
        }
        if !status.is_success() {
            bail!("looking up releases at {url} failed: {status}");
        }
        let body = response
            .bytes()
            .await
            .with_context(|| format!("read releases from {url}"))?;
        serde_json::from_slice(&body)
            .map(Some)
            .with_context(|| format!("parse releases from {url}"))
    }

    /// Downloads `url` into `path`; returns its size and SHA-256.
    async fn download(&self, url: &str, path: &Path) -> anyhow::Result<(u64, String)> {
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("download {url}"))?;
        let mut file =
            fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("download {url}"))?
        {
            hasher.update(&chunk);
            file.write_all(&chunk)
                .with_context(|| format!("write {}", path.display()))?;
            size += chunk.len() as u64;
        }
        file.sync_all()
            .with_context(|| format!("write {}", path.display()))?;
        Ok((size, hex(&hasher.finalize())))
    }

    async fn text(&self, url: &str) -> anyhow::Result<String> {
        self.client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("download {url}"))?
            .text()
            .await
            .with_context(|| format!("download {url}"))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The path of the binary this process runs, with symlinks resolved, so the
/// file is replaced and not a link to it.
fn running_executable() -> anyhow::Result<PathBuf> {
    let path = std::env::current_exe().context("find this executable")?;
    fs::canonicalize(&path).with_context(|| {
        format!(
            "find this executable at {}; if it was replaced since it started, run the new one",
            path.display()
        )
    })
}

/// Downloads `candidate` beside `executable`, checks it, and moves it over
/// `executable`. Nothing is replaced unless every check passed.
async fn install(
    releases: &Releases,
    candidate: &Candidate<'_>,
    executable: &Path,
) -> anyhow::Result<()> {
    let directory = executable
        .parent()
        .context("the executable has no directory")?;
    // Beside the binary, on its filesystem, so the final rename is atomic.
    let scratch = tempfile::Builder::new()
        .prefix(".cookie-update-")
        .tempdir_in(directory)
        .with_context(|| {
            format!(
                "cannot write to {}, where cookie is installed; rerun with permission to \
                 write there",
                directory.display()
            )
        })?;
    let archive = scratch.path().join(&candidate.archive.name);
    let (size, digest) = releases
        .download(&candidate.archive.browser_download_url, &archive)
        .await?;
    let expected = releases
        .text(&candidate.checksum.browser_download_url)
        .await?;
    verify_checksum(&expected, &digest, &candidate.archive.name)?;
    println!(
        "downloaded {} ({:.1} MB), sha256 ok",
        candidate.archive.name,
        size as f64 / 1_000_000.0
    );
    let binary = {
        let (archive, into) = (archive.clone(), scratch.path().to_owned());
        tokio::task::spawn_blocking(move || extract_binary(&archive, &into))
            .await
            .context("extract the archive")??
    };
    copy_permissions(executable, &binary)?;
    smoke_test(&binary, &candidate.version).await?;
    replace(&binary, executable)
}

/// Checks the `.sha256` file's digest, the first word of its text, against
/// the download's.
fn verify_checksum(file: &str, digest: &str, name: &str) -> anyhow::Result<()> {
    let expected = file
        .split_whitespace()
        .next()
        .filter(|word| word.len() == 64 && word.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .with_context(|| format!("{name}.sha256 holds no SHA-256 digest"))?;
    if !expected.eq_ignore_ascii_case(digest) {
        bail!("{name} does not match its published SHA-256; nothing was replaced");
    }
    Ok(())
}

/// The binary's file name inside a release archive.
const BINARY: &str = if cfg!(windows) {
    "cookie.exe"
} else {
    "cookie"
};

/// Extracts the binary from the release archive at `archive` into `into`.
#[cfg(unix)]
fn extract_binary(archive: &Path, into: &Path) -> anyhow::Result<PathBuf> {
    // The decoder writes to a stream, so the tar goes to disk on the way.
    let tar_path = into.join("archive.tar");
    {
        let mut input = io::BufReader::new(
            fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?,
        );
        let mut output = io::BufWriter::new(
            fs::File::create(&tar_path)
                .with_context(|| format!("create {}", tar_path.display()))?,
        );
        lzma_rs::xz_decompress(&mut input, &mut output)
            .map_err(|error| anyhow!("decompress {}: {error}", archive.display()))?;
        output
            .flush()
            .with_context(|| format!("write {}", tar_path.display()))?;
    }
    let mut tar = tar::Archive::new(
        fs::File::open(&tar_path).with_context(|| format!("open {}", tar_path.display()))?,
    );
    let destination = into.join(BINARY);
    for entry in tar.entries().context("read the archive")? {
        let mut entry = entry.context("read the archive")?;
        let is_binary = entry.header().entry_type().is_file()
            && entry.path().context("read the archive")?.file_name()
                == Some(std::ffi::OsStr::new(BINARY));
        if is_binary {
            write_binary(&mut entry, &destination)?;
            fs::remove_file(&tar_path).ok();
            return Ok(destination);
        }
    }
    bail!("the archive has no `{BINARY}`")
}

/// Extracts the binary from the release archive at `archive` into `into`.
#[cfg(windows)]
fn extract_binary(archive: &Path, into: &Path) -> anyhow::Result<PathBuf> {
    let file = fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(io::BufReader::new(file)).context("read the archive")?;
    let destination = into.join(BINARY);
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).context("read the archive")?;
        let is_binary = entry.is_file()
            && entry
                .name()
                .context("read the archive")?
                .rsplit(['/', '\\'])
                .next()
                == Some(BINARY);
        if is_binary {
            write_binary(&mut entry, &destination)?;
            return Ok(destination);
        }
    }
    bail!("the archive has no `{BINARY}`")
}

fn write_binary(entry: &mut impl io::Read, destination: &Path) -> anyhow::Result<()> {
    let mut file = fs::File::create(destination)
        .with_context(|| format!("create {}", destination.display()))?;
    io::copy(entry, &mut file).context("extract the binary")?;
    file.sync_all()
        .with_context(|| format!("write {}", destination.display()))
}

/// Gives `binary` the installed binary's mode, executable in any case.
#[cfg(unix)]
fn copy_permissions(installed: &Path, binary: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = fs::metadata(installed)
        .with_context(|| format!("read {}", installed.display()))?
        .permissions()
        .mode();
    fs::set_permissions(binary, fs::Permissions::from_mode((mode & 0o7777) | 0o111))
        .with_context(|| format!("make {} executable", binary.display()))
}

#[cfg(windows)]
fn copy_permissions(_installed: &Path, _binary: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Runs `binary --version` and requires a cookie of `version`'s
/// major.minor.patch: a wrong architecture, a missing system library, or a
/// truncated file fails here while the installed binary is untouched.
/// Releases built before the channel was stamped print only that core.
async fn smoke_test(binary: &Path, version: &ReleaseVersion) -> anyhow::Result<()> {
    let (major, minor, patch) = version.core;
    let core = format!("{major}.{minor}.{patch}");
    let output = tokio::time::timeout(
        SMOKE_TEST_TIMEOUT,
        tokio::process::Command::new(binary)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "the downloaded binary did not print its version within {} s; nothing was replaced",
            SMOKE_TEST_TIMEOUT.as_secs()
        )
    })?
    .with_context(|| "run the downloaded binary; nothing was replaced")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || !prints_version(&stdout, &core) {
        bail!(
            "the downloaded binary does not run here ({}{}); nothing was replaced",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .map(|line| format!(": {line}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// Whether `stdout` of `cookie --version` names release `core`
/// (major.minor.patch) as its version: `cookie <version> (...)`, where the
/// version is `core` itself or `core` with a prerelease or build suffix.
/// `1.2.3` does not match `1.2.30` or `11.2.3`.
fn prints_version(stdout: &str, core: &str) -> bool {
    let Some(version) = stdout
        .strip_prefix("cookie ")
        .and_then(|rest| rest.split_whitespace().next())
    else {
        return false;
    };
    version
        .strip_prefix(core)
        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(['-', '+']))
}

/// Moves `binary` over `executable` in one rename. Processes running the
/// old binary keep it.
#[cfg(unix)]
fn replace(binary: &Path, executable: &Path) -> anyhow::Result<()> {
    fs::rename(binary, executable).with_context(|| format!("replace {}", executable.display()))?;
    if let Some(directory) = executable.parent()
        && let Ok(directory) = fs::File::open(directory)
    {
        directory.sync_all().ok();
    }
    Ok(())
}

/// Moves `binary` over `executable`. A running `.exe` cannot be replaced
/// but can be renamed: it moves aside under a name
/// [`remove_replaced_binaries`] deletes once nothing runs it.
#[cfg(windows)]
fn replace(binary: &Path, executable: &Path) -> anyhow::Result<()> {
    let aside = executable.with_file_name(format!(
        "{}.{}.old",
        executable_name(executable),
        uuid::Uuid::now_v7().simple()
    ));
    fs::rename(executable, &aside)
        .with_context(|| format!("move {} aside", executable.display()))?;
    if let Err(error) = fs::rename(binary, executable) {
        fs::rename(&aside, executable).ok();
        return Err(error).with_context(|| format!("replace {}", executable.display()));
    }
    fs::remove_file(&aside).ok();
    Ok(())
}

#[cfg(windows)]
fn executable_name(executable: &Path) -> String {
    executable
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| BINARY.to_owned())
}

/// Deletes binaries an update moved aside, now that the processes that ran
/// them may have exited; one still running stays until a later start.
#[cfg(windows)]
pub(crate) fn remove_replaced_binaries() {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let (Some(directory), prefix) = (
        executable.parent(),
        format!("{}.", executable_name(&executable)),
    ) else {
        return;
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".old") {
            fs::remove_file(entry.path()).ok();
        }
    }
}

/// Where the installers write their receipt.
fn receipt_path() -> Option<PathBuf> {
    let directory = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else {
        match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
            Some(config) => PathBuf::from(config),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
        }
    };
    Some(directory.join("cookie_agent/cookie_agent-receipt.json"))
}

/// Sets the receipt's `version` when the receipt at `path` is for the
/// binary at `executable`; `Ok(false)` when there is none or it is for
/// another install.
fn update_receipt(path: &Path, executable: &Path, version: &str) -> io::Result<bool> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut receipt: serde_json::Value = serde_json::from_str(&text).map_err(io::Error::other)?;
    let Some(prefix) = receipt
        .get("install_prefix")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(false);
    };
    let prefix = fs::canonicalize(prefix).unwrap_or_else(|_| PathBuf::from(prefix));
    if !executable.starts_with(&prefix) {
        return Ok(false);
    }
    receipt["version"] = serde_json::Value::String(version.to_owned());
    let directory = path.parent().unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(serde_json::to_string(&receipt)?.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(true)
}

/// Says when the local daemon still runs another version. It is never
/// restarted here: the user restarts it when nothing they care about runs.
async fn report_daemon(version: &str) {
    let Ok(configuration) = cookie_agent_config::load() else {
        return;
    };
    let probe =
        crate::local_daemon::probe(&configuration.runtime.server, super::client_identity("cli"))
            .await;
    if let crate::local_daemon::Probe::Connected(daemon) = probe
        && let Some(hint) = daemon_hint(&daemon.hello.engine_version, version)
    {
        println!("{hint}");
    }
}

fn daemon_hint(daemon_version: &str, installed: &str) -> Option<String> {
    (daemon_version != installed).then(|| {
        format!(
            "the local daemon still runs {daemon_version}; restart it when convenient:\n  \
             cookie daemon restart"
        )
    })
}

#[cfg(test)]
mod tests;
