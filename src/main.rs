// #![allow(unused)]

use axum::{
    body::{Bytes, StreamBody},
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use bytesize::ByteSize;
use dashmap::DashMap;
use mime_types::MIME_TYPES;
use minify_html::Cfg;
use minify_js::Session;
use notify::{Event, Watcher};
use notify_deb::{new_debouncer, DebounceEventResult};
use os_str_bytes::RawOsStr;
use path_clean::PathClean;
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt::Debug,
    fs::create_dir_all,
    io::{self, stderr, Read, Seek, Stderr, Write},
    net::SocketAddr,
    ops::Deref,
    path::PathBuf,
    sync::{mpsc::sync_channel, Arc},
    time::Duration,
};
use tokio::{fs::File, join, task::JoinHandle, time::Instant};
use tokio_util::io::ReaderStream;
use tracing::{level_filters::LevelFilter, *};
use tracing_appender::rolling::{RollingFileAppender, RollingWriter};
use tracing_subscriber::fmt::{format::FmtSpan, MakeWriter};
use walkdir::WalkDir;

mod mime_types;

const SOCKETADDR: ([u8; 4], u16) = ([127, 0, 0, 1], 3000);

#[derive(Clone)]
enum FileData {
    /// The bytes of the cached file + the mime-type string
    Cached(Bytes),
    References(Arc<PathBuf>),
    Processing(Instant),
    Uncached(PathBuf),
}

impl Debug for FileData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use FileData::*;
        match self {
            Cached(cache) => write!(f, "Cached({})", ByteSize::b(cache.len() as u64)),
            References(target) => write!(f, "Links to {target:?}"),
            Processing(start) => write!(f, "Processing({:?})", start.elapsed()),
            Uncached(target) => write!(f, "Disk({target:?})"),
        }
    }
}

// #[derive(Debug, Clone)]
// pub struct Config {
//     path: &'static str,
//     path_os: std::path::PathBuf,
// }

type FileMap = &'static DashMap<Arc<PathBuf>, FileData>;

#[derive(Debug, Clone)]
// Use the notify crate to create a watcher task that maintains the state
pub struct AppState {
    map: FileMap, // config: &'static ArcSwap<Config>,
}

#[derive(Debug)]
struct Logger<W> {
    file_writer: RollingFileAppender,
    output: W,
}

type Out = Stderr;
const OUT_INIT: fn() -> Out = stderr;

struct StdoutWrapper {
    out: Out,
}

impl StdoutWrapper {
    fn new() -> Self {
        Self { out: OUT_INIT() }
    }
}

impl Write for StdoutWrapper {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.out.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.out.write_all(buf)
    }
    fn write_fmt(&mut self, fmt: std::fmt::Arguments<'_>) -> std::io::Result<()> {
        self.out.write_fmt(fmt)
    }
    fn write_vectored(&mut self, bufs: &[std::io::IoSlice<'_>]) -> std::io::Result<usize> {
        self.out.write_vectored(bufs)
    }
}

impl Clone for StdoutWrapper {
    fn clone(&self) -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct LogWriter<'a, W> {
    file_writer: RollingWriter<'a>,
    output: W,
}

impl<W> Logger<W> {
    pub fn new(file_writer: RollingFileAppender, writer: W) -> Self {
        Self {
            file_writer,
            output: writer,
        }
    }
}

impl<'a, W> Write for LogWriter<'a, W>
where
    W: Write,
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let bytes = self.file_writer.write(buf)?;
        self.output.write_all(&buf[..bytes])?;
        Ok(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file_writer.flush()?;
        self.output.flush()
    }
}

impl<'a, W: Write + Clone> MakeWriter<'a> for Logger<W> {
    type Writer = LogWriter<'a, W>;

    fn make_writer(&'a self) -> Self::Writer {
        let Logger {
            file_writer,
            output,
        } = self;
        let file_writer = file_writer.make_writer();
        let output = output.clone();
        LogWriter {
            file_writer,
            output,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FileState {
    /// File deleted, or moved from that location
    Deleted,
    /// File modified, or created
    Modified,
}

/// The file prefix used for hiding files
const HIDDEN_PREFIX: &str = ".";

/// The maximum filesize we allow caching
const MAX_FILESIZE: u64 = 1024_u64.pow(2) * 20;

fn cache<P: AsRef<std::path::Path>>(path: P) -> bool {
    let path = path.as_ref();
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    meta.len() < MAX_FILESIZE || {
        let Some(ext) = path.extension() else {
            return false;
        };
        let ext = RawOsStr::new(ext);
        let e = ext.as_ref();
        (e == "html" || e == "js" || e == "css") && meta.len() < MAX_FILESIZE * 2
    }
}

#[derive(Debug, Clone)]
struct CacheSettings<'a> {
    path: Cow<'a, std::path::Path>,
    prefix: Option<Cow<'a, OsStr>>,
}

impl<'a> CacheSettings<'a> {
    pub fn new<P: Into<Cow<'a, std::path::Path>>, F: Into<Cow<'a, OsStr>>>(
        path: P,
        prefix: Option<F>,
    ) -> Self {
        let path = path.into().into();
        let prefix = prefix.map(|i| i.into());
        CacheSettings { path, prefix }
    }
    pub fn ensure_path_exists(&self) -> bool {
        if self.path.is_dir() {
            return true;
        };
        create_dir_all(&self.path).is_ok()
    }
}

fn path_to_cachefile_noprefix<'a, P: AsRef<std::path::Path>>(
    path: P,
    cache_settings: &'a CacheSettings,
) -> PathBuf {
    let cache_settings = {
        let CacheSettings { path, .. } = cache_settings;
        CacheSettings {
            path: path.as_ref().into(),
            prefix: None,
        }
    };
    path_to_cachefile(path, &cache_settings)
}

fn path_to_cachefile<'a, P: AsRef<std::path::Path>>(
    path: P,
    cache_settings: &'a CacheSettings,
) -> PathBuf {
    let path = path.as_ref();
    let mut cache_file_path = cache_settings.path.to_path_buf();
    cache_file_path.push(path);
    if let Some(prefix) = &cache_settings.prefix {
        let name = cache_file_path.file_name().unwrap_or_default();
        let mut cache_filename = OsString::with_capacity(
            name.len()
                + cache_settings
                    .prefix
                    .as_ref()
                    .map(|s| s.len())
                    .unwrap_or_default(),
        );
        cache_filename.push(prefix);
        cache_filename.push(name);
        cache_file_path.set_file_name(cache_filename);
    }
    cache_file_path
}
const TAIL_BYTE: u8 = b'\n';
/// Check whether the hash contained in file at path and the hash of file at check match, and
/// return a handle to the cache file if they match
///
/// WARNING: the returned handle will be moved after the last byte of the hash, if you need to
/// overwrite it, make sure to seek to the beginning!
///
/// Some(Err()) and None values indicate that you should run clear_file_cache
fn check_hash<'a, P: AsRef<std::path::Path> + Debug, P2: AsRef<std::path::Path> + Debug>(
    path: P,
    // The path of the file to compare the hash of
    check: P2,
    cache_settings: &'a CacheSettings,
) -> Option<Result<std::fs::File, std::fs::File>> {
    let cachepath = path_to_cachefile(path, cache_settings);
    let mut buf = [0; 32];
    let Ok(mut cache_file) = std::fs::File::open(&cachepath) else {
            debug!("Couild not find cache file at {cachepath:?}");
            return None
        };
    if let Err(err) = cache_file.read_exact(&mut buf) {
        info!("Failed to read hash from cache file at {cachepath:?}: {err}");
        return None;
    };
    let mut tail_check = [0];
    let _ = cache_file.read_exact(&mut tail_check);
    if tail_check != [TAIL_BYTE] {
        warn!("Tail byte didn't match in cache file at {cachepath:?}");
        return None;
    };
    let Ok(mut check_file) = std::fs::File::open(&check) else {
            info!("Failed to open file at {check:?}");
            return None
        };
    let mut hasher = Sha256::new();
    if let Err(err) = io::copy(&mut check_file, &mut hasher) {
        info!("Failed to hash file at {check_file:?}: {err}");
        let _ = check_file.seek(io::SeekFrom::Start(0));
        return Some(Err(check_file));
    };
    let hash = &hasher.finalize()[..];
    if buf == hash {
        // Hashes match
        return Some(Ok(cache_file));
    }
    // Hashes don't match
    let _ = check_file.seek(io::SeekFrom::Start(0));
    Some(Err(check_file))
}
fn cache_file<'a, P: AsRef<std::path::Path>, T: AsRef<std::path::Path> + Debug>(
    path: P,
    data: impl AsRef<[u8]>,
    source: T,
    cache_settings: &'a CacheSettings,
) {
    let path = path.as_ref();
    let source = source.as_ref();
    const MIN_CACHE_BYTES: usize = 0;
    let data = data.as_ref();
    if data.len() < MIN_CACHE_BYTES {
        return;
    };
    let cache_file_path = path_to_cachefile(path, cache_settings);
    if let Some(folder) = cache_file_path.parent() {
        if let Err(err) = std::fs::create_dir_all(folder) {
            error!(
                "Failed to create folder hieararchy {folder:?} for cache file {:?}: {err}",
                cache_file_path.file_name().unwrap_or_default()
            );
            return;
        }
    }
    let Ok(mut cache_file) = std::fs::File::create(&cache_file_path) else {
            error!("Failed to create cache file at {cache_file_path:?}");
            return
        };
    if let Err(err) = cache_file.set_len(0) {
        error!("Failed to truncate file at {cache_file:?}: {err}");
        return;
    };
    let Ok(mut check_file) = std::fs::File::open(source) else {
            info!("Failed to open file at {source:?}");
            return
        };
    let mut hasher = Sha256::new();
    if let Err(err) = io::copy(&mut check_file, &mut hasher) {
        info!("Failed to hash file at {check_file:?}: {err}");
        return;
    };
    let hash = hasher.finalize();
    let Ok(()) = cache_file.write_all(&hash) else {
            error!("Failed to write hash to cache file at {cache_file_path:?}");
            return
        };
    let Ok(()) = cache_file.write_all(&[TAIL_BYTE]) else {
            error!("Failed to write hash tail byte to cache file at {cache_file_path:?}");
            return
        };
    let Ok(()) = cache_file.write_all(data) else {
            error!("Failed to write hash tail byte to cache file at {cache_file_path:?}");
            return
        };
    cache_file.sync_data().unwrap()
}
#[instrument(level = "debug")]
fn clear_file_cache<'a, P: AsRef<std::path::Path> + Debug>(
    path: P,
    cache_settings: &'a CacheSettings,
) {
    let cache_file_path = path_to_cachefile(&path, cache_settings);
    let cache_file_path_noprefix = path_to_cachefile_noprefix(&path, cache_settings);
    let span = debug_span!(
        "Cleaning cache",
        path = format!("{cache_file_path:?}, {cache_file_path_noprefix:?}")
    )
    .entered();
    let mut success = std::fs::remove_dir_all(&cache_file_path).is_ok();
    success |= std::fs::remove_dir_all(&cache_file_path_noprefix).is_ok();
    success |= std::fs::remove_file(&cache_file_path).is_ok();
    span.exit();
    if success {
        debug!("Removed cache file/dir at {cache_file_path:?}");
    }
}

#[instrument(level = "debug")]
fn handle_path_deletion<'a>(
    path: PathBuf,
    path_prefix_len: usize,
    index_filename: &OsStr,
    cache_settings: &'a CacheSettings<'a>,
    map: FileMap,
) {
    let path = path.clean();
    let mut path: PathBuf = path.into_iter().skip(path_prefix_len).collect();
    clear_file_cache(&path, cache_settings);
    map.retain(|k, _| !k.starts_with(&path));
    // map.iter().map(|r| r.pair()).filter(|(&p, _)| p.starts_with(base));
    // Remove prefix from path
    if let Some((_, removed)) = map.remove(&path) {
        if matches!(removed, FileData::Cached(_)) {}
    };
    if path.file_name() == Some(index_filename) {
        // Index file case
        // Remove index name from path
        path.pop();
        // Remove index based file from path
        map.remove(&path);
    }
}
#[instrument(level = "debug")]
fn handle_path<'a>(
    path: PathBuf,
    path_prefix_len: usize,
    index_filename: &OsStr,
    cache_settings: &'a CacheSettings<'a>,
    map: FileMap,
) {
    let path = path.clean();
    // Ignore hidden files
    if path
        .file_name()
        .map(|name| RawOsStr::new(name))
        .is_some_and(|name| name.starts_with(HIDDEN_PREFIX))
    {
        return;
    };
    // Remove prefix from path
    let original_path = path;
    let mut path: PathBuf = original_path.iter().skip(path_prefix_len).collect();
    path.shrink_to_fit();
    let path = Arc::new(path);
    map.insert(path.clone(), FileData::Processing(Instant::now()));
    let data = match check_hash(path.as_ref(), &original_path, cache_settings) {
        // Cache file matches
        Some(Ok(mut cache)) => {
            debug!("Hash matched for {path:?}");
            let mut buf = Vec::new();
            if let Err(err) = cache.read_to_end(&mut buf) {
                warn!("Encountered error while reading cache file: {err}");
                return;
            };
            FileData::Cached(Bytes::from(buf))
        }
        // Cache file doesn't match
        other => {
            if !cache(&original_path) {
                FileData::Uncached(original_path)
            } else {
                let mut file_data = match other {
                    Some(Err(source)) => source,
                    _ => {
                        let Ok(file_data) = std::fs::File::open(&original_path) else {
            debug!("Failed to open file {original_path:?}. Likely caused by file deletion after modification on a debounce boundary");
            return;
                };
                        file_data
                    }
                };
                let mut buf = Vec::new();
                file_data.read_to_end(&mut buf).unwrap();
                // Minify JS and HTML
                let mut cache = false;
                if let Some(ext) = path.extension() {
                    if ext == "html" || ext == "css" {
                        cache |= true;
                        buf = minify_html::minify(&buf, &Cfg::new())
                    } else if ext == "js" {
                        cache |= true;
                        let session = Session::new();
                        let mut out = Vec::with_capacity(buf.len());
                        if minify_js::minify(
                            &session,
                            minify_js::TopLevelMode::Global,
                            &buf,
                            &mut out,
                        )
                        .is_ok()
                        {
                            buf = out;
                        };
                    }
                }
                buf.shrink_to_fit();
                if cache {
                    cache_file(&*path, &buf, original_path, cache_settings);
                }
                FileData::Cached(Bytes::from(buf))
            }
        }
    };
    if path.file_name() == Some(index_filename) {
        // Index file case
        let mut shortpath = path.deref().clone();
        // Remove index name from path
        shortpath.pop();
        shortpath.shrink_to_fit();
        let shortpath = Arc::new(shortpath);
        // Insert reference to file
        let _ = map.insert(shortpath, FileData::References(path.clone()));
        let _ = map.insert(path, data);
        return;
    }
    let _ = map.insert(path, data);
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The path to get the files we're going to host from
    let cache_settings = CacheSettings::new(std::path::Path::new("cache"), Some(OsStr::new("c_")));
    assert!(
        cache_settings.ensure_path_exists(),
        "Failed to read/initialize cache path directory"
    );
    let path: &str = "public";
    let path_os = std::path::Path::new(path).canonicalize().expect("Failed to open target path, are you sure you're in the right directory and the target is readable by the user?");
    let path_prefix_len = path_os.iter().count();
    // let config = Box::leak(Box::new(ArcSwap::from_pointee(Config {
    //     path,
    //     path_os: path_os.clone(),
    // })));

    // Set up tracing
    let file_writer = tracing_appender::rolling::daily("logs", "log");
    let writer = Logger::new(file_writer, StdoutWrapper::new());
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_span_events(FmtSpan::CLOSE)
        .with_max_level(LevelFilter::INFO)
        // .with_line_number(true)
        .with_writer(writer)
        .init();

    // build our application with a route
    let map: FileMap = Box::leak(Box::new(DashMap::with_capacity(10)));
    let state = AppState { map, /* config */ };
    let app = Router::new()
        // `GET /` goes to `root`
        .route("/", get(root))
        .route("/*key", get(get_file))
        .with_state(state);

    // run our app with hyper, listening globally on port 3000
    let addr = SocketAddr::from(SOCKETADDR);

    let axum_server = axum::Server::bind(&addr).serve(app.into_make_service());
    let file_watcher: JoinHandle<notify::Result<()>> = tokio::spawn(async move {
        let span = debug_span!("Creating debouncer and fs notifier").entered();
        let (tx, rx) = sync_channel(8);
        let mut debouncer = new_debouncer(
            Duration::from_millis(1000 / 2),
            None,
            move |res: DebounceEventResult| {
                if let Ok(events) = res {
                    match tx.send(events) {
                        Ok(()) => (),
                        Err(_) => {
                            error!(
                                "Event reciever has been dropped, this really should not happen."
                            );
                            panic!()
                        }
                    }
                }
            },
        )?;
        span.exit();

        let index_filename: &OsStr = OsStr::new("index.html");
        let span = info_span!("Checking for dead files in cache").entered();
        for entry in WalkDir::new(&cache_settings.path)
            .follow_links(false)
            .max_open(10)
            .contents_first(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let original_path = entry.path();
            if entry.metadata().is_ok_and(|meta| meta.is_dir()) {
                // Attempt to clean up any empty cache dir left behind
                let _ = std::fs::remove_dir(path);
                continue;
            }
            let Ok(path)=original_path.strip_prefix(&cache_settings.path) else {
                error!("Failed to remove prefix path from {path:?}... Oh no.");
                continue
            };

            // SAFETY: Should be fine as the only time the name is None is when the path
            // terminaltes in .., which cannot happen here
            let mut final_path = path
                .parent()
                .map(|path| path_os.join(path))
                .unwrap_or(path.to_path_buf());
            // let path = path_os.join(.unwrap_or(OsStr::new(".")));
            let name = path.file_name().unwrap().to_os_string();
            if let Some(prefix) = &cache_settings.prefix {
                let s = RawOsStr::new(&name).to_owned();
                let Some(name) = s
                    .strip_prefix(prefix.to_str().unwrap()).map(|s|s.to_os_str()) else {
                warn!("Could not remove cache prefix from {original_path:?}. Deleting it.");
                if std::fs::remove_file(original_path).is_ok() {
                    info!("Deleted {original_path:?} successfully")
                };
                continue
                    };
                final_path.push(&name)
            } else {
                final_path.push(name)
            }
            let path = final_path;
            if !path.exists() {
                info!("Found dead cache file {original_path:?}. Deleting it.");
                if std::fs::remove_file(original_path).is_ok() {
                    info!("Deleted {original_path:?} successfully")
                };
            }
        }
        span.exit();
        let span = info_span!("Registering files").entered();
        for entry in WalkDir::new(&path_os)
            .max_open(10)
            .follow_links(true)
            .into_iter()
            .filter_entry(|entry| !RawOsStr::new(entry.file_name()).starts_with(HIDDEN_PREFIX))
            .filter_map(|entry| entry.ok())
        {
            if entry.file_type().is_dir() {
                // Ignore directories
                continue;
            }
            let Ok(_meta) = entry.metadata() else {
                // Ignore files we can't get metadata for
                continue;
            };
            let path = entry.into_path();
            handle_path(path, path_prefix_len, index_filename, &cache_settings, map)
        }
        span.exit();
        info!("Registered files: {map:?}");

        // Add a path to be watched. All files and directories at that path and
        // below will be monitored for changes.
        debouncer
            .watcher()
            .watch(std::path::Path::new(path), notify::RecursiveMode::Recursive)?;
        info!("Registered filesystem watcher");
        let mut change_map = HashMap::new();
        for events in rx {
            let span = debug_span!("Handling filesystem notifications").entered();
            for event in events {
                let Event {
                    kind,
                    paths,
                    attrs: _,
                } = event;
                use notify::EventKind::*;
                use FileState::*;
                let mut paths = paths.into_iter();
                let Some(first) = paths.next() else {
                    // Ignore events with no paths
                    continue;
                };
                let second = paths.next();
                match kind {
                    // Index
                    Create(notify::event::CreateKind::File) => {
                        // Mark file as invalidated
                        change_map.insert(first, Modified);
                    }
                    Remove(_kind) => {
                        // Mark file as deleted
                        change_map.insert(first, Deleted);
                    }
                    Modify(kind) => {
                        // Mark file as invalidated
                        use notify::event::ModifyKind;
                        use notify::event::RenameMode;
                        match kind {
                            ModifyKind::Data(_)
                            | ModifyKind::Metadata(_)
                            | ModifyKind::Any
                            | ModifyKind::Other => {
                                change_map.insert(first, Modified);
                            }
                            ModifyKind::Name(RenameMode::From) => {
                                change_map.insert(first, Deleted);
                            }
                            ModifyKind::Name(RenameMode::To) => {
                                change_map.insert(first, Modified);
                            }
                            ModifyKind::Name(RenameMode::Both)
                            | ModifyKind::Name(RenameMode::Any | RenameMode::Other) => {
                                change_map.insert(first, Deleted);
                                if let Some(second) = second {
                                    change_map.insert(second, Modified);
                                }
                            }
                        }
                    }
                    // Ignore other events
                    _ => (),
                }
            }
            span.exit();
            let span = debug_span!(
                "Changes detected, syncing to filesystem",
                "{:?}",
                change_map
            )
            .entered();
            for (path, change) in change_map.drain() {
                match change {
                    FileState::Modified => {
                        handle_path(path, path_prefix_len, index_filename, &cache_settings, map)
                    }
                    FileState::Deleted => handle_path_deletion(
                        path,
                        path_prefix_len,
                        index_filename,
                        &cache_settings,
                        map,
                    ),
                }
            }
            span.exit();
            info!("After filesystem sync: {map:?}")
        }
        Ok(())
    });
    info!("Listening on {addr}");
    let _ = join!(axum_server, file_watcher);
    Ok(())
}

// basic handler that responds with a static string
// #[instrument(skip(state))]
#[instrument(level = "debug")]
#[axum::debug_handler]
async fn get_file(State(state): State<AppState>, Path(path): Path<PathBuf>) -> impl IntoResponse {
    let mut path = Arc::new(path);
    let res = 'main: loop {
        use FileData::*;
        let Some(file) = state.map.get(&path) else {
            return ( StatusCode::NOT_FOUND,"Not Found.\n",).into_response()
        };
        let file: &FileData = file.deref();
        // Loop in case we need to traverse references
        match file {
            Uncached(path) => {
                // Get file from filesystem
                let Ok(file) = File::open(path).await else {
                break None
            };
                // convert the `AsyncRead` into a `Stream`
                let stream = ReaderStream::new(file);
                // convert the `Stream` into an `axum::body::HttpBody`
                let body = StreamBody::new(stream);
                // "asd".into_response()
                break Some(
                    Response::builder()
                        .header(header::CONTENT_TYPE, get_ext(path))
                        .body(body)
                        .unwrap()
                        .into_response(),
                );
            }
            Cached(bytes) => {
                break Some(
                    Response::builder()
                        .header(header::CONTENT_TYPE, get_ext(path.as_ref()))
                        .body(bytes.clone().into_response())
                        .unwrap()
                        .into_response(),
                )
            }
            References(newpath) => {
                path = newpath.clone();
            }
            Processing(proc_start) => {
                // Wait for processing to complete
                debug!(
                    "Waiting for processing of {path:?}({:?}) to finish.",
                    proc_start.elapsed()
                );
                const TIMEOUT: Duration = Duration::from_secs(1);
                let start = Instant::now();
                let mut interval = tokio::time::interval(Duration::from_millis(50));
                // interval.tick().await
                while start.elapsed() < TIMEOUT {
                    interval.tick().await;
                    let Some(data) = state.map.get(&path) else {
                        info!("The FileData of {path:?}({:?}) disappeared while waiting for its processing to finish", proc_start.elapsed());
                        break 'main None
                    };
                    let data = data.deref();
                    if !matches!(data, Processing(_)) {
                        continue 'main;
                    }
                }
                error!(
                    "Waiting for the FileData of {path:?}({:?}) to finish processing timed out",
                    proc_start.elapsed()
                );
                break 'main None;
            }
        }
    };
    let Some(res) = res else {
return ( StatusCode::NOT_FOUND,"Not Found.\n",).into_response()
    };
    res
}

#[instrument(level = "debug")]
fn get_ext<P: AsRef<std::path::Path> + Debug>(path: P) -> &'static str {
    let path = path.as_ref();
    let ext = path.extension().map(|ext| ext.to_string_lossy());
    ext.map(|ext| MIME_TYPES.get(&ext))
        .flatten()
        .map(|&ext| ext)
        .unwrap_or("text/plain")
}

// basic handler that responds with a static string
async fn root(state: State<AppState>) -> impl IntoResponse {
    get_file(state, Path(PathBuf::new())).await
}

// async fn flatten<T, E>(handle: JoinHandle<Result<T, E>>) -> Result<T, E> {
//     match handle.await {
//         Ok(Ok(result)) => Ok(result),
//         Ok(Err(err)) => Err(err),
//         Err(err) => {
//             error!("Failed to spawn a task in a flatten call: {err:?}");
//             panic!("Failed to spawn")
//         }
//     }
// }
