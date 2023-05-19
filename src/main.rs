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
use std::{
    collections::HashMap,
    ffi::OsStr,
    fmt::Debug,
    io::{stderr, Read, Stderr, Write},
    net::SocketAddr,
    ops::Deref,
    path::PathBuf,
    sync::{mpsc::sync_channel, Arc},
    time::Duration,
};
use tokio::{fs::File, join, task::JoinHandle, time::Instant};
use tokio_util::io::ReaderStream;
use tracing::{error, info, info_span, instrument, metadata::LevelFilter, warn};
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
const MAX_FILESIZE: u64 = 1024_u64.pow(2) * 8;

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
        (e == "html" || e == "js" || e == "css") && meta.len() < MAX_FILESIZE * 4
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The path to get the files we're going to host from
    let path: &str = "public";
    let path_os = std::path::Path::new(path).canonicalize().unwrap();
    let path_prefix_len = path_os.iter().count();
    // let config = Box::leak(Box::new(ArcSwap::from_pointee(Config {
    //     path,
    //     path_os: path_os.clone(),
    // })));

    // Set up tracing
    let file_writer = tracing_appender::rolling::daily("logs", "log");
    let writer = Logger::new(file_writer, StdoutWrapper::new());
    let _subscriber = tracing_subscriber::fmt()
        .with_ansi(true)
        .with_span_events(FmtSpan::CLOSE)
        .with_max_level(LevelFilter::WARN)
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

    #[instrument(level = "debug")]
    fn handle_path_deletion(
        path: PathBuf,
        path_prefix_len: usize,
        index_filename: &OsStr,
        map: FileMap,
    ) {
        let path = path.clean();
        let mut path: PathBuf = path.into_iter().skip(path_prefix_len).collect();
        // Remove prefix from path
        map.remove(&path);
        if path.file_name() == Some(index_filename) {
            // Index file case
            // Remove index name from path
            path.pop();
            // Remove index based file from path
            map.remove(&path);
        }
    }
    #[instrument(level = "debug")]
    fn handle_path(path: PathBuf, path_prefix_len: usize, index_filename: &OsStr, map: FileMap) {
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
        if cache(&original_path) {
            let Ok(mut file_data) = std::fs::File::open(&original_path) else {
            info!("Failed to open file {original_path:?}. Likely caused by file deletion after modification on a debounce boundary");
            return;
        };
            map.insert(path.clone(), FileData::Processing(Instant::now()));

            let mut buf = Vec::new();
            file_data.read_to_end(&mut buf).unwrap();
            // Minify JS and HTML
            if let Some(ext) = path.extension() {
                if ext == "html" || ext == "css" {
                    buf = minify_html::minify(&buf, &Cfg::new())
                } else if ext == "js" {
                    let session = Session::new();
                    let mut out = Vec::with_capacity(buf.len());
                    if minify_js::minify(&session, minify_js::TopLevelMode::Global, &buf, &mut out)
                        .is_ok()
                    {
                        buf = out;
                    };
                }
            }
            buf.shrink_to_fit();
            if path.file_name() == Some(index_filename) {
                // Index file case
                let mut shortpath = path.deref().clone();
                // Remove index name from path
                shortpath.pop();
                shortpath.shrink_to_fit();
                let shortpath = Arc::new(shortpath);
                // Insert reference to file
                let _ = map.insert(shortpath, FileData::References(path.clone()));
                let _ = map.insert(path, FileData::Cached(Bytes::from(buf)));
                return;
            }
            let _ = map.insert(path, FileData::Cached(Bytes::from(buf)));
        } else {
            map.insert(path.clone(), FileData::Processing(Instant::now()));
            if path.file_name() == Some(index_filename) {
                // Index file case
                let mut shortpath = path.deref().clone();
                // Remove index name from path
                shortpath.pop();
                shortpath.shrink_to_fit();
                let shortpath = Arc::new(shortpath);
                // Insert reference to file
                let _ = map.insert(shortpath, FileData::References(path.clone()));
                let _ = map.insert(path, FileData::Uncached(original_path));
                return;
            }
            let _ = map.insert(path, FileData::Uncached(original_path));
        }
    }

    info!("Listening on {}", addr);
    let axum_server = axum::Server::bind(&addr).serve(app.into_make_service());
    let file_watcher: JoinHandle<notify::Result<()>> = tokio::spawn(async move {
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

        let index_filename: &OsStr = OsStr::new("index.html");
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
            handle_path(path, path_prefix_len, index_filename, map)
        }
        drop(span);
        info!("Registered files, map: {map:?}");

        // Add a path to be watched. All files and directories at that path and
        // below will be monitored for changes.
        debouncer
            .watcher()
            .watch(std::path::Path::new(path), notify::RecursiveMode::Recursive)?;
        let mut change_map = HashMap::new();
        for events in rx {
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
            let span = info_span!("Changes detected, syncing to filesystem").entered();
            for (path, change) in change_map.drain() {
                match change {
                    FileState::Modified => handle_path(path, path_prefix_len, index_filename, map),
                    FileState::Deleted => {
                        handle_path_deletion(path, path_prefix_len, index_filename, map)
                    }
                }
            }
            drop(span)
        }
        Ok(())
    });
    let _ = join!(axum_server, file_watcher);
    Ok(())
}

// basic handler that responds with a static string
// #[instrument(skip(state))]
#[instrument(level = "info")]
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
                info!(
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
                        error!("The FileData of {path:?}({:?}) disappeared while waiting for its processing to finish",

                    proc_start.elapsed()
                               );
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

#[instrument(level = "info")]
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
