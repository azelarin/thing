//! remote and refreshed images against a private loopback http server. the
//! server answers each path deterministically and records every request and
//! every connection the client abandoned. the system curl is the helper, as
//! in production; nothing leaves the loopback interface.

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use super::*;

const WAIT: Duration = Duration::from_secs(10);

/// a private directory removed when the test ends. thing-ui has no
/// temporary-file dependency, and adding one would change the lock file.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "thing-image-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
enum Reply {
    Png([u8; 4]),
    Status(u16),
    Redirect(&'static str),
    /// a png sent only after `release`; a client that leaves is recorded.
    Held([u8; 4]),
    /// headers promising a body that never comes.
    Stall,
    Bytes(usize),
    /// a body without a length, ended by closing the connection.
    Unsized(usize),
    /// each request gets the next color, so a refresh is visible.
    Sequence(Vec<[u8; 4]>),
}

#[derive(Default)]
struct Log {
    requests: Vec<String>,
    abandoned: Vec<String>,
}

struct Server {
    address: SocketAddr,
    log: Arc<Mutex<Log>>,
    release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Server {
    fn start(routes: Vec<(&'static str, Reply)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Log::default()));
        let release = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let routes: Arc<HashMap<&'static str, Reply>> = Arc::new(routes.into_iter().collect());
        let counters = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        {
            let (log, release, stop) = (log.clone(), release.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (log, release, routes, counters, stop) = (
                                log.clone(),
                                release.clone(),
                                routes.clone(),
                                counters.clone(),
                                stop.clone(),
                            );
                            thread::spawn(move || {
                                serve(stream, &routes, &log, &release, &counters, &stop)
                            });
                        }
                        Err(_) => thread::sleep(Duration::from_millis(2)),
                    }
                }
            });
        }
        Self {
            address,
            log,
            release,
            stop,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn requests(&self, path: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|request| *request == path)
            .count()
    }

    fn abandoned(&self, path: &str) -> bool {
        self.log
            .lock()
            .unwrap()
            .abandoned
            .iter()
            .any(|request| request == path)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.release.store(true, Ordering::SeqCst);
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn png(color: [u8; 4]) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(4, 2, image::Rgba(color));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    bytes.into_inner()
}

fn serve(
    mut stream: TcpStream,
    routes: &HashMap<&'static str, Reply>,
    log: &Mutex<Log>,
    release: &AtomicBool,
    counters: &Mutex<HashMap<String, usize>>,
    stop: &AtomicBool,
) {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    }
    let text = String::from_utf8_lossy(&request);
    let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
    log.lock().unwrap().requests.push(path.clone());
    let respond = |stream: &mut TcpStream, status: &str, headers: &str, body: &[u8]| {
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
    };
    let reply = routes
        .get(path.as_str())
        .cloned()
        .unwrap_or(Reply::Status(404));
    match reply {
        Reply::Png(color) => respond(&mut stream, "200 OK", "", &png(color)),
        Reply::Status(code) => respond(&mut stream, &format!("{code} Nope"), "", b""),
        Reply::Redirect(to) => respond(
            &mut stream,
            "302 Found",
            &format!("Location: {to}\r\n"),
            b"",
        ),
        Reply::Bytes(count) => respond(&mut stream, "200 OK", "", &vec![b'x'; count]),
        Reply::Unsized(count) => {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(&vec![b'x'; count]);
        }
        Reply::Sequence(colors) => {
            let index = {
                let mut counters = counters.lock().unwrap();
                let count = counters.entry(path.clone()).or_default();
                *count += 1;
                *count - 1
            };
            respond(
                &mut stream,
                "200 OK",
                "",
                &png(colors[index.min(colors.len() - 1)]),
            );
        }
        reply @ (Reply::Held(_) | Reply::Stall) => {
            let body = match reply {
                Reply::Held(color) => png(color),
                _ => Vec::new(),
            };
            let length = if body.is_empty() { 100 } else { body.len() };
            let head =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(head.as_bytes());
            stream.set_nonblocking(true).unwrap();
            loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                // a closed connection reads as end of file.
                match stream.read(&mut buffer) {
                    Ok(0) => {
                        log.lock().unwrap().abandoned.push(path);
                        return;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => {
                        log.lock().unwrap().abandoned.push(path);
                        return;
                    }
                }
                if release.load(Ordering::SeqCst) && !body.is_empty() {
                    stream.set_nonblocking(false).unwrap();
                    let _ = stream.write_all(&body);
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn limits() -> remote::Limits {
    remote::Limits {
        connect: Duration::from_secs(2),
        overall: Duration::from_secs(4),
        stall: Duration::from_secs(1),
        direct: true,
        ..remote::Limits::default()
    }
}

fn images(limits: remote::Limits) -> Images {
    Images::with_limits(|| {}, limits).unwrap()
}

fn image(source: &str, revision: Option<&str>) -> Image {
    Image {
        source: Some(source.into()),
        revision: revision.map(Into::into),
        ..Image::default()
    }
}

fn bounds(size: f32) -> Rect {
    Rect {
        width: size,
        height: size,
        ..Rect::default()
    }
}

/// collects and asks again until the image is ready, returning its first
/// pixel as rgba.
fn ready(images: &mut Images, image: &Image, size: f32) -> [u8; 4] {
    let deadline = Instant::now() + WAIT;
    loop {
        images.collect();
        if let Some((skia, _, _)) = images.get(image, ImageFit::Fill, bounds(size), 1.0) {
            let pixmap = skia.peek_pixels().unwrap();
            let color = pixmap.get_color((0, 0));
            return [color.r(), color.g(), color.b(), color.a()];
        }
        assert!(
            Instant::now() < deadline,
            "image {image:?} never became ready"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// collects until the request at `size` has failed.
fn failed(images: &mut Images, image: &Image, size: f32) {
    let deadline = Instant::now() + WAIT;
    loop {
        images.collect();
        assert!(
            images
                .get(image, ImageFit::Fill, bounds(size), 1.0)
                .is_none()
        );
        let key = request_key(image, ImageFit::Fill, bounds(size), 1.0);
        if matches!(
            images.cache.get(&key).map(|entry| &entry.state),
            Some(State::Failed)
        ) {
            return;
        }
        assert!(Instant::now() < deadline, "image {image:?} never failed");
        thread::sleep(Duration::from_millis(5));
    }
}

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

#[test]
fn remote_images_load_once_and_decode_again_for_new_sizes() {
    let server = Server::start(vec![("/red.png", Reply::Png(RED))]);
    let mut images = images(limits());
    let red = image(&server.url("/red.png"), None);
    assert_eq!(ready(&mut images, &red, 16.0), RED);
    // a new size decodes the downloaded bytes again without another request.
    assert_eq!(ready(&mut images, &red, 48.0), RED);
    assert_eq!(server.requests("/red.png"), 1);
    // a redirect is followed to the image.
    let server = Server::start(vec![
        ("/moved", Reply::Redirect("/red.png")),
        ("/red.png", Reply::Png(RED)),
    ]);
    let moved = image(&server.url("/moved"), None);
    assert_eq!(ready(&mut images, &moved, 16.0), RED);
    assert_eq!(server.requests("/red.png"), 1);
}

#[test]
fn a_new_revision_refetches_and_a_superseded_download_cannot_return() {
    let server = Server::start(vec![
        ("/art.png", Reply::Sequence(vec![RED, GREEN, BLUE])),
        ("/slow.png", Reply::Held(RED)),
    ]);
    let mut images = images(limits());
    let url = server.url("/art.png");
    assert_eq!(ready(&mut images, &image(&url, Some("1")), 16.0), RED);
    // the same revision is cached; a new one asks the server again.
    assert_eq!(ready(&mut images, &image(&url, Some("1")), 32.0), RED);
    assert_eq!(ready(&mut images, &image(&url, Some("2")), 16.0), GREEN);
    assert_eq!(server.requests("/art.png"), 2);

    // a slow first revision is cancelled by the second, and its connection
    // is abandoned rather than finishing later.
    let slow = server.url("/slow.png");
    images.collect();
    assert!(
        images
            .get(&image(&slow, Some("a")), ImageFit::Fill, bounds(16.0), 1.0)
            .is_none()
    );
    let deadline = Instant::now() + WAIT;
    while server.requests("/slow.png") == 0 {
        assert!(Instant::now() < deadline, "the slow request never arrived");
        thread::sleep(Duration::from_millis(5));
    }
    let second = image(&slow, Some("b"));
    images.retain_sources([&second]);
    assert!(
        images
            .get(&second, ImageFit::Fill, bounds(16.0), 1.0)
            .is_none()
    );
    while !server.abandoned("/slow.png") {
        assert!(
            Instant::now() < deadline,
            "the superseded download kept running"
        );
        thread::sleep(Duration::from_millis(5));
    }
    server.release.store(true, Ordering::SeqCst);
    assert_eq!(ready(&mut images, &second, 16.0), RED);
    let stale = request_key(&image(&slow, Some("a")), ImageFit::Fill, bounds(16.0), 1.0);
    assert!(!images.cache.contains_key(&stale));
}

#[test]
fn widgets_can_load_different_revisions_of_the_same_url_together() {
    let server = Server::start(vec![("/shared.png", Reply::Held(RED))]);
    let mut images = images(limits());
    let first = image(&server.url("/shared.png"), Some("first"));
    let second = image(&server.url("/shared.png"), Some("second"));
    images.get(&first, ImageFit::Fill, bounds(16.0), 1.0);
    let deadline = Instant::now() + WAIT;
    while server.requests("/shared.png") == 0 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    images.get(&second, ImageFit::Fill, bounds(16.0), 1.0);
    images.retain_sources([&first, &second]);
    assert_eq!(
        images.remote.running.len(),
        2,
        "one widget cancelled the other's image"
    );
    server.release.store(true, Ordering::SeqCst);
    assert_eq!(ready(&mut images, &first, 16.0), RED);
    assert_eq!(ready(&mut images, &second, 16.0), RED);
    assert_eq!(server.requests("/shared.png"), 2);
}

#[test]
fn failures_are_final_until_the_revision_changes() {
    let server = Server::start(vec![
        ("/gone.png", Reply::Status(404)),
        ("/loop", Reply::Redirect("/loop")),
        ("/large.png", Reply::Bytes(8192)),
        ("/notes.txt", Reply::Bytes(16)),
        ("/unsized.png", Reply::Unsized(65536)),
    ]);
    let mut images = images(remote::Limits {
        max_bytes: 4096,
        ..limits()
    });
    let gone = image(&server.url("/gone.png"), None);
    failed(&mut images, &gone, 16.0);
    // another size and later frames do not retry it.
    failed(&mut images, &gone, 32.0);
    images.collect();
    assert!(
        images
            .get(&gone, ImageFit::Fill, bounds(16.0), 1.0)
            .is_none()
    );
    assert_eq!(server.requests("/gone.png"), 1);
    failed(
        &mut images,
        &image(&server.url("/gone.png"), Some("again")),
        16.0,
    );
    assert_eq!(server.requests("/gone.png"), 2);

    // redirects, size, and content all have limits.
    failed(&mut images, &image(&server.url("/loop"), None), 16.0);
    assert!(
        server.requests("/loop") <= 4,
        "{}",
        server.requests("/loop")
    );
    failed(&mut images, &image(&server.url("/large.png"), None), 16.0);
    failed(&mut images, &image(&server.url("/notes.txt"), None), 16.0);
    // without a declared length, thing's own limit stops the transfer.
    failed(&mut images, &image(&server.url("/unsized.png"), None), 16.0);
}

#[test]
fn a_stalled_server_neither_blocks_local_images_nor_outlives_its_limits() {
    let server = Server::start(vec![("/stall.png", Reply::Stall)]);
    let directory = TempDir::new();
    let local = directory.path().join("local.png");
    std::fs::write(&local, png(BLUE)).unwrap();
    let mut images = images(limits());
    let stalled = image(&server.url("/stall.png"), None);
    images.collect();
    assert!(
        images
            .get(&stalled, ImageFit::Fill, bounds(16.0), 1.0)
            .is_none()
    );
    // the local file decodes while the download is stuck.
    let started = Instant::now();
    assert_eq!(
        ready(&mut images, &image(local.to_str().unwrap(), None), 16.0),
        BLUE
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    // the stall limit then ends the download.
    failed(&mut images, &stalled, 16.0);
}

#[test]
fn clearing_and_dropping_cancel_downloads() {
    let server = Server::start(vec![("/held.png", Reply::Held(RED))]);
    let mut images = images(limits());
    let held = image(&server.url("/held.png"), None);
    images.collect();
    assert!(
        images
            .get(&held, ImageFit::Fill, bounds(16.0), 1.0)
            .is_none()
    );
    let deadline = Instant::now() + WAIT;
    while server.requests("/held.png") == 0 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    // a reload clears the cache and kills the helper.
    images.clear();
    while !server.abandoned("/held.png") {
        assert!(
            Instant::now() < deadline,
            "clearing left the download running"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(images.remote.running.is_empty());

    // shutdown does the same without waiting for the server.
    let server = Server::start(vec![("/held.png", Reply::Held(RED))]);
    let held = image(&server.url("/held.png"), None);
    images.collect();
    assert!(
        images
            .get(&held, ImageFit::Fill, bounds(16.0), 1.0)
            .is_none()
    );
    while server.requests("/held.png") == 0 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let started = Instant::now();
    drop(images);
    assert!(started.elapsed() < Duration::from_millis(500));
    while !server.abandoned("/held.png") {
        assert!(
            Instant::now() < deadline,
            "shutdown left the download running"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn queued_downloads_and_kept_bytes_stay_bounded() {
    let routes: Vec<(&'static str, Reply)> = (0..24)
        .map(|index| {
            let path: &'static str = Box::leak(format!("/{index}.png").into_boxed_str());
            (path, Reply::Png(RED))
        })
        .chain([("/held.png", Reply::Held(GREEN))])
        .collect();
    let server = Server::start(routes);
    let mut images = images(remote::Limits {
        workers: 1,
        queue: 2,
        ..limits()
    });
    // one worker is busy and the queue holds two; later requests are not
    // remembered as pending and are asked for again on a later frame.
    let held = image(&server.url("/held.png"), None);
    images.collect();
    assert!(
        images
            .get(&held, ImageFit::Fill, bounds(16.0), 1.0)
            .is_none()
    );
    let deadline = Instant::now() + WAIT;
    while images.remote.fetcher.as_ref().unwrap().active() != 1 || server.requests("/held.png") == 0
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    for index in 0..4 {
        let queued = image(&server.url(&format!("/{index}.png")), None);
        assert!(
            images
                .get(&queued, ImageFit::Fill, bounds(16.0), 1.0)
                .is_none()
        );
    }
    assert_eq!(images.remote.running.len(), 3);
    assert_eq!(images.remote.fetcher.as_ref().unwrap().active(), 3);
    server.release.store(true, Ordering::SeqCst);

    for index in 0..24 {
        let path = format!("/{index}.png");
        assert_eq!(
            ready(&mut images, &image(&server.url(&path), None), 8.0),
            RED
        );
    }
    assert!(images.remote.downloads.len() <= DOWNLOAD_ENTRIES);
    assert!(images.remote.downloaded_bytes() <= DOWNLOAD_BYTES);
}

#[test]
fn completed_downloads_wait_for_the_ui_before_more_bodies_are_loaded() {
    let server = Server::start(vec![("/red.png", Reply::Png(RED))]);
    let mut fetcher = remote::Fetcher::start(
        remote::Limits {
            workers: 1,
            queue: 3,
            ..limits()
        },
        Arc::new(|| {}),
    )
    .unwrap();
    let mut ids = Vec::new();
    for revision in ["a", "b", "c"] {
        ids.push(
            fetcher
                .submit(remote::Identity {
                    url: server.url("/red.png"),
                    revision: Some(revision.into()),
                })
                .unwrap(),
        );
    }
    let deadline = Instant::now() + WAIT;
    for id in ids {
        while fetcher.completed() == 0 {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        // leave the result uncollected while other requests are already queued.
        thread::sleep(Duration::from_millis(50));
        let done = fetcher.take_done();
        assert_eq!(done.len(), 1, "completed bodies exceeded the worker budget");
        assert_eq!(done[0].0, id);
        assert!(done[0].2.is_ok());
    }
    assert_eq!(server.requests("/red.png"), 3);
}

#[test]
fn helper_errors_do_not_expose_redirect_credentials() {
    use std::os::unix::fs::PermissionsExt;
    let directory = TempDir::new();
    let helper = directory.path().join("curl");
    std::fs::write(&helper, "#!/bin/sh\nread -r line\nprintf '%s' 'https://user:secret@redirect.example/image?token=private' >&2\nexit 60\n").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut fetcher = remote::Fetcher::start(
        remote::Limits {
            program: helper,
            ..limits()
        },
        Arc::new(|| {}),
    )
    .unwrap();
    fetcher
        .submit(remote::Identity {
            url: "https://origin.example/image".into(),
            revision: None,
        })
        .unwrap();
    let deadline = Instant::now() + WAIT;
    while fetcher.completed() == 0 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let done = fetcher.take_done();
    assert_eq!(
        done[0].2.as_ref().unwrap_err(),
        "the image server's TLS certificate could not be verified"
    );
}

#[test]
fn local_files_refresh_by_revision_including_atomic_replacement() {
    let directory = TempDir::new();
    let path = directory.path().join("cover art.png");
    std::fs::write(&path, png(RED)).unwrap();
    let mut images = images(limits());
    let source = path.to_str().unwrap();
    assert_eq!(ready(&mut images, &image(source, None), 16.0), RED);

    // an atomic rename replaces the file; the same revision stays cached and
    // a new one shows the replacement.
    let staged = directory.path().join("staged.png");
    std::fs::write(&staged, png(GREEN)).unwrap();
    std::fs::rename(&staged, &path).unwrap();
    assert_eq!(ready(&mut images, &image(source, None), 16.0), RED);
    assert_eq!(ready(&mut images, &image(source, Some("2")), 16.0), GREEN);
    // so does writing in place.
    std::fs::write(&path, png(BLUE)).unwrap();
    assert_eq!(ready(&mut images, &image(source, Some("3")), 16.0), BLUE);

    // mpris-style file urls name the same file, with percent escapes.
    let url = format!("file://{}", source.replace(' ', "%20"));
    assert_eq!(ready(&mut images, &image(&url, Some("3")), 16.0), BLUE);
    assert!(location("file://elsewhere/art.png").is_err());
    assert!(
        images.remote.fetcher.is_none(),
        "local images never start downloads"
    );
}
