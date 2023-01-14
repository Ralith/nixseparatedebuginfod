use crate::db::{Cache, Entry, Timestamp};
use crate::log::ResultExt;
use anyhow::Context;
use futures_util::{future::join_all, stream::FuturesOrdered, FutureExt, StreamExt};
use object::read::Object;
use once_cell::unsync::Lazy;
use sqlx::{sqlite::SqliteConnectOptions, ConnectOptions, Connection, Row};
use std::sync::Arc;
use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    os::unix::prelude::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};
use tokio::sync::{mpsc::Sender, Semaphore};
use tokio::task::JoinHandle;

pub async fn realise(path: &Path) -> anyhow::Result<()> {
    use tokio::fs::metadata;
    use tokio::process::Command;
    if metadata(path).await.is_ok() {
        return Ok(());
    };
    let mut command = Command::new("nix-store");
    command.arg("--realise").arg(path);
    log::info!("Running {:?}", &command);
    let _ = command.status().await;
    if metadata(path).await.is_ok() {
        return Ok(());
    };
    anyhow::bail!("nix-store --realise {} failed", path.display());
}

/// Walks a store path and attempts to register everything that has a buildid in it.
fn register_store_path(storepath: &Path, sendto: Sender<Entry>) {
    log::debug!("examining {}", storepath.display());
    if storepath
        .file_name()
        .unwrap_or_default()
        .as_bytes()
        .ends_with(b".drv")
    {
        return;
    }
    if !storepath.is_dir() {
        return;
    }
    let deriver_source = Lazy::new(|| match get_deriver(storepath) {
        Err(e) => {
            log::info!("no deriver for {}: {:#}", storepath.display(), e);
            (None, None)
        }
        Ok(deriver) => {
            if deriver.is_file() {
                let source = match get_source(deriver.as_path()) {
                    Err(e) => {
                        log::info!(
                            "no source for {} (deriver of {}): {:#}",
                            deriver.display(),
                            storepath.display(),
                            e
                        );
                        None
                    }
                    Ok(s) => Some(s),
                };
                (Some(deriver), source)
            } else {
                (None, None)
            }
        }
    });
    let storepath_os: &OsStr = storepath.as_ref();
    if storepath_os.as_bytes().ends_with(b"-debug") {
        let mut root = storepath.to_owned();
        root.push("lib");
        root.push("debug");
        root.push(".build-id");
        if !root.is_dir() {
            return;
        };
        let readroot = match std::fs::read_dir(&root) {
            Err(e) => {
                log::warn!("could not list {}: {:#}", root.display(), e);
                return;
            }
            Ok(r) => r,
        };
        for mid in readroot {
            let mid = match mid {
                Err(e) => {
                    log::warn!("could not list {}: {:#}", root.display(), e);
                    continue;
                }
                Ok(mid) => mid,
            };
            if !mid.file_type().map(|x| x.is_dir()).unwrap_or(false) {
                continue;
            };
            let mid_path = mid.path();
            let mid_name_os = mid.file_name();
            let mid_name = match mid_name_os.to_str() {
                None => continue,
                Some(x) => x,
            };
            let read_mid = match std::fs::read_dir(&mid_path) {
                Err(e) => {
                    log::warn!("could not list {}: {:#}", mid_path.display(), e);
                    continue;
                }
                Ok(r) => r,
            };
            for end in read_mid {
                let end = match end {
                    Err(e) => {
                        log::warn!("could not list {}: {:#}", mid_path.display(), e);
                        continue;
                    }
                    Ok(end) => end,
                };
                if !end.file_type().map(|x| x.is_file()).unwrap_or(false) {
                    continue;
                };
                let end_name_os = end.file_name();
                let end_name = match end_name_os.to_str() {
                    None => continue,
                    Some(x) => x,
                };
                if !end_name.ends_with(".debug") {
                    continue;
                };
                let buildid = format!(
                    "{}{}",
                    &mid_name,
                    &end_name[..(end_name.len() - ".debug".len())]
                );
                let (_, source) = &*deriver_source;
                let entry = Entry {
                    debuginfo: end.path().to_str().map(|s| s.to_owned()),
                    executable: None,
                    source: source
                        .as_ref()
                        .and_then(|path| path.to_str().map(|s| s.to_owned())),
                    buildid,
                };
                sendto
                    .blocking_send(entry)
                    .context("sending entry failed")
                    .or_warn();
            }
        }
    } else {
        let debug_output = Lazy::new(|| {
            let (deriver, _) = &*deriver_source;
            match deriver {
                None => None,
                Some(deriver) => match get_debug_output(deriver.as_path()) {
                    Ok(None) => None,
                    Err(e) => {
                        log::warn!(
                            "could not determine if the deriver {} of {} has a debug output: {:#}",
                            storepath.display(),
                            deriver.display(),
                            e
                        );
                        None
                    }
                    Ok(Some(d)) => Some(d),
                },
            }
        });
        for file in walkdir::WalkDir::new(storepath) {
            let file = match file {
                Err(_) => continue,
                Ok(file) => file,
            };
            if !file.file_type().is_file() {
                continue;
            };
            let path = file.path();
            let buildid = match get_buildid(path) {
                Err(e) => {
                    log::info!("cannot get buildid of {}: {:#}", path.display(), e);
                    continue;
                }
                Ok(Some(buildid)) => buildid,
                Ok(None) => continue,
            };
            let debuginfo = match &*debug_output {
                None => None,
                Some(storepath) => {
                    let theoretical = debuginfo_path_for(&buildid, storepath.as_path());
                    if storepath.is_dir() {
                        // the store path is available, check the prediction
                        if !theoretical.is_file() {
                            log::warn!(
                                "{} has buildid {}, and {} exists but not {}",
                                path.display(),
                                buildid,
                                storepath.display(),
                                theoretical.display()
                            );
                            None
                        } else {
                            Some(theoretical)
                        }
                    } else {
                        Some(theoretical)
                    }
                }
            };
            let (_, source) = &*deriver_source;
            let entry = Entry {
                buildid,
                source: source
                    .as_ref()
                    .and_then(|path| path.to_str().map(|s| s.to_owned())),
                executable: path.to_str().map(|s| s.to_owned()),
                debuginfo: debuginfo.and_then(|path| path.to_str().map(|s| s.to_owned())),
            };
            sendto
                .blocking_send(entry)
                .context("sending entry failed")
                .or_warn();
        }
    }
}

/// Return the path where separate debuginfo is to be found in a debug output for a buildid
fn debuginfo_path_for(buildid: &str, debug_output: &Path) -> PathBuf {
    let mut res = debug_output.to_path_buf();
    res.push("lib");
    res.push("debug");
    res.push(".build-id");
    res.push(&buildid[..2]);
    res.push(format!("{}.debug", &buildid[2..]));
    res
}

/// Obtains the derivation of a store path.
///
/// The store path must exist.
fn get_deriver(storepath: &Path) -> anyhow::Result<PathBuf> {
    let mut cmd = std::process::Command::new("nix-store");
    cmd.arg("--query").arg("--deriver").arg(storepath);
    log::debug!("Running {:?}", &cmd);
    let out = cmd.output().with_context(|| format!("running {:?}", cmd))?;
    if !out.status.success() {
        anyhow::bail!("{:?} failed: {}", cmd, String::from_utf8_lossy(&out.stderr));
    }
    let n = out.stdout.len();
    if n <= 1 || out.stdout[n - 1] != b'\n' {
        anyhow::bail!(
            "{:?} returned weird output: {}",
            cmd,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let path = PathBuf::from(OsString::from_vec(out.stdout[..n - 1].to_owned()));
    if !path.is_absolute() {
        // nix returns `unknown-deriver` when it does not know
        anyhow::bail!("no deriver");
    };
    Ok(path)
}

/// Obtains the debug output corresponding to this derivation
///
/// The derivation must exist.
fn get_debug_output(drvpath: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mut cmd = std::process::Command::new("nix-store");
    cmd.arg("--query").arg("--outputs").arg(drvpath);
    log::debug!("Running {:?}", &cmd);
    let out = cmd.output().with_context(|| format!("running {:?}", cmd))?;
    if !out.status.success() {
        anyhow::bail!("{:?} failed: {}", cmd, String::from_utf8_lossy(&out.stderr));
    }
    for output in out.stdout.split(|&elt| elt == b'\n') {
        if output.ends_with(b"-debug") {
            return Ok(Some(PathBuf::from(OsString::from_vec(output.to_owned()))));
        }
    }
    return Ok(None);
}

/// Obtains the source store path corresponding to this derivation
///
/// The derivation must exist.
///
/// Source is understood as `src = `, multiple sources or patches are not supported.
fn get_source(drvpath: &Path) -> anyhow::Result<PathBuf> {
    let mut cmd = std::process::Command::new("nix-store");
    cmd.arg("--query").arg("--binding").arg("src").arg(drvpath);
    log::debug!("Running {:?}", &cmd);
    let out = cmd.output().with_context(|| format!("running {:?}", cmd))?;
    if !out.status.success() {
        anyhow::bail!("{:?} failed: {}", cmd, String::from_utf8_lossy(&out.stderr));
    }
    let n = out.stdout.len();
    if n <= 1 || out.stdout[n - 1] != b'\n' {
        anyhow::bail!(
            "{:?} returned weird output: {}",
            cmd,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let path = PathBuf::from(OsString::from_vec(out.stdout[..n - 1].to_owned()));
    if !path.is_absolute() {
        // nix returns `unknown-deriver` when it does not know
        anyhow::bail!("no deriver");
    };
    Ok(path)
}

/// Where a source file might be
#[derive(Debug, Clone)]
pub enum SourceLocation {
    /// Inside an archive
    Archive {
        /// path of the archive
        archive: PathBuf,
        /// path of the file in the archive
        member: PathBuf,
    },
    /// A file directly in the store
    File(PathBuf),
}

impl SourceLocation {
    /// Get the path against which we match the requested file name
    fn member_path(&self) -> &Path {
        match self {
            SourceLocation::Archive { member, .. } => member.as_path(),
            SourceLocation::File(path) => path.as_path(),
        }
    }
}

/// Return the build id of this file.
///
/// If the file is not an executable returns Ok(None).
/// Errors are only for errors returned from the fs.
fn get_buildid(path: &Path) -> anyhow::Result<Option<String>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {} to get its buildid", path.display()))?;
    let reader = object::read::ReadCache::new(file);
    let object = match object::read::File::parse(&reader) {
        Err(_) => {
            // object::read::Error is opaque, so no way to distinguish "this is not elf" and a real
            // error
            return Ok(None);
        }
        Ok(o) => o,
    };
    match object
        .build_id()
        .with_context(|| format!("parsing {} for buildid", path.display()))?
    {
        None => Ok(None),
        Some(data) => {
            let buildid = base16::encode_lower(&data);
            Ok(Some(buildid))
        }
    }
}

const BATCH_SIZE: usize = 100;
const N_WORKERS: usize = 8;

#[derive(Clone)]
pub struct StoreWatcher {
    cache: &'static Cache,
    semaphore: Arc<Semaphore>,
}

impl StoreWatcher {
    pub fn new(cache: &'static Cache) -> Self {
        Self {
            cache,
            semaphore: Arc::new(Semaphore::new(N_WORKERS)),
        }
    }

    pub async fn maybe_register_new_paths(&self) -> anyhow::Result<Option<JoinHandle<()>>> {
        let timestamp = self
            .cache
            .get_registration_timestamp()
            .await
            .context("reading cache timestamp")?;
        let (paths, timestamp) = get_new_store_path_batch(timestamp)
            .await
            .context("looking for new paths registered in the nix store")?;
        if paths.is_empty() {
            Ok(None)
        } else {
            let cloned_self = self.clone();
            Ok(Some(tokio::spawn(async move {
                cloned_self.register_new_paths(paths, timestamp).await
            })))
        }
    }

    async fn register_store_path(&self, path: PathBuf, sendto: Sender<Entry>) {
        let path2 = path.clone();
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("closed semaphore");
        tokio::task::spawn_blocking(move || {
            register_store_path(path.as_path(), sendto);
            drop(permit);
        })
        .await
        .with_context(|| format!("examining {} failed", path2.as_path().display()))
        .or_warn();
    }

    async fn register_new_paths(&self, paths: Vec<PathBuf>, timestamp: Timestamp) {
        let (entries_tx, mut entries_rx) = tokio::sync::mpsc::channel(3 * BATCH_SIZE);
        let batch: Vec<_> = paths
            .into_iter()
            .map(|path| self.register_store_path(path, entries_tx.clone()))
            .collect();
        let batch_handle = join_all(batch).map(move |_| timestamp).boxed();
        let mut last_timestamp = timestamp;
        let mut unfinished_batches = FuturesOrdered::new();
        unfinished_batches.push_back(batch_handle);
        let mut entry_buffer = Vec::with_capacity(BATCH_SIZE);
        let mut get_new_batches = true;
        loop {
            tokio::select! {
                entry = entries_rx.recv() => {
                    match entry {
                        Some(entry) => {
                            entry_buffer.push(entry);
                            if entry_buffer.len() >= BATCH_SIZE {
                                match self.cache.register(&entry_buffer).await {
                                    Ok(()) => entry_buffer.clear(),
                                    Err(e) => log::warn!("cannot write entries to sqlite db: {:#}", e),
                                }
                            }
                        },
                        None => log::warn!("entries_rx closed"),
                    }
                }
                timestamp = unfinished_batches.next() => {
                    match timestamp {
                        Some(timestamp) => {
                            match self.cache.register(&entry_buffer).await {
                                Ok(()) => {
                                    entry_buffer.clear();
                                    self.cache.set_registration_timestamp(timestamp).await.context("writing registration timestamp").or_warn();
                                    log::debug!("batch {} ok", timestamp);
                                },
                                Err(e) => log::warn!("cannot write entries to sqlite db: {:#}", e),
                            }
                        },
                        None => {
                            // there are no more running batches
                            self.cache.register(&entry_buffer).await.context("registering entries").or_warn();
                            entry_buffer.clear();
                            log::info!("Done registering new store paths");
                            return;
                        },
                    }
                }
            }
            if get_new_batches && self.semaphore.available_permits() > 0 {
                log::debug!("starting a new batch of store paths to register");
                let (paths, timestamp) = match get_new_store_path_batch(last_timestamp).await {
                    Ok(x) => x,
                    Err(e) => {
                        log::warn!("cannot read nix store db: {:#}", e);
                        continue;
                    }
                };
                let batch: Vec<_> = paths
                    .into_iter()
                    .map(|path| self.register_store_path(path, entries_tx.clone()))
                    .collect();
                if batch.is_empty() {
                    log::debug!("batch is empty");
                    get_new_batches = false;
                } else {
                    let batch_handle = join_all(batch).map(move |_| timestamp).boxed();
                    last_timestamp = timestamp;
                    unfinished_batches.push_back(batch_handle);
                }
            }
        }
    }
}

async fn get_new_store_path_batch(
    from_timestamp: Timestamp,
) -> anyhow::Result<(Vec<PathBuf>, Timestamp)> {
    // note: this is a hack. One cannot open a sqlite db read only with WAL if the underlying
    // file is not writable. So we promise sqlite that the db will not be modified with
    // immutable=1, but it's false.
    let mut db = SqliteConnectOptions::new()
        .filename("/nix/var/nix/db/db.sqlite")
        .immutable(true)
        .read_only(true)
        .connect()
        .await
        .context("opening nix db")?;
    let rows =
        sqlx::query("select path, registrationTime from ValidPaths where registrationTime >= $1 and registrationTime <= (with candidates(registrationTime) as (select registrationTime from ValidPaths where registrationTime >= $1 order by registrationTime asc limit 100) select max(registrationTime) from candidates)")
            .bind(from_timestamp)
            .fetch_all(&mut db)
            .await
            .context("reading nix db")?;
    let mut paths = Vec::new();
    let mut max_time = 0;
    for row in rows {
        let path: &str = row.try_get("path").context("parsing path in nix db")?;
        if !path.starts_with("/nix/store") || path.chars().filter(|&x| x == '/').count() != 3 {
            anyhow::bail!(
                "read corrupted stuff from nix db: {}, concurrent write?",
                path
            );
        }
        paths.push(PathBuf::from(path));
        let time: Timestamp = row
            .try_get("registrationTime")
            .context("parsing timestamp in nix db")?;
        max_time = time.max(max_time);
    }
    // As we lie about the database being immutable let's not keep the connection open
    db.close().await.context("closing nix db").or_warn();
    if (max_time == 0) ^ paths.is_empty() {
        anyhow::bail!("read paths with 0 registration time...");
    }
    Ok((paths, max_time + 1))
}

/// Attempts to find a file that matches the request in an existing source path.
pub fn get_file_for_source(
    source: &Path,
    request: &Path,
) -> anyhow::Result<Option<SourceLocation>> {
    let target: Vec<&OsStr> = request.iter().collect();
    // invariant: we only keep candidates which have same path as target for components i..
    let mut i = target.len() - 1;
    let mut candidates: Vec<_> = Vec::new();
    let source_type = source
        .metadata()
        .with_context(|| format!("stat({})", source.display()))?;
    if source_type.is_dir() {
        for file in walkdir::WalkDir::new(source) {
            match file {
                Err(e) => {
                    log::warn!("failed to walk source {}: {:#}", source.display(), e);
                    continue;
                }
                Ok(f) => {
                    if Some(&f.file_name()) == target.get(i) {
                        candidates.push(SourceLocation::File(f.path().to_path_buf()));
                    }
                }
            }
        }
    } else if source_type.is_file() {
        let mut archive = std::fs::File::open(&source)
            .with_context(|| format!("opening source archive {}", source.display()))?;
        let member_list = compress_tools::list_archive_files(&mut archive)
            .with_context(|| format!("listing files in source archive {}", source.display()))?;
        for member in member_list {
            if Path::new(&member).file_name().as_ref() == target.get(i) {
                candidates.push(SourceLocation::Archive {
                    archive: source.to_path_buf(),
                    member: PathBuf::from(member),
                });
            }
        }
    }
    if candidates.len() == 0 {
        return Ok(None);
    }
    if candidates.len() == 1 {
        return Ok(Some(candidates[0].clone()));
    }
    let candidates_split: HashSet<(usize, Vec<&OsStr>)> = candidates
        .iter()
        .map(|p| p.member_path().iter().collect())
        .enumerate()
        .collect();
    let mut candidates_ref: HashSet<&(usize, Vec<&OsStr>)> = candidates_split.iter().collect();
    while candidates_ref.len() >= 2 && i > 0 {
        i -= 1;
        let next: HashSet<&(usize, Vec<&OsStr>)> = candidates_ref
            .iter()
            .cloned()
            .filter(|&(_, ref c)| c.get(i) == target.get(i))
            .collect();
        if next.len() == 0 {
            anyhow::bail!(
                "cannot tell {:?} apart from {} for target {}",
                &candidates_ref,
                &source.display(),
                request.display()
            );
        };
        candidates_ref = next;
    }
    let (winner, _) = candidates_ref.iter().next().unwrap();
    Ok(Some(candidates[*winner].clone()))
}
