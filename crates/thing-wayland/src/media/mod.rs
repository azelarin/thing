//! mpris player discovery, state, and controls.

mod model;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use async_io::{Async, Timer};
use calloop::ping::Ping;
use futures_lite::future;
use futures_util::{
    FutureExt, StreamExt,
    future::LocalBoxFuture,
    stream::{self, FuturesUnordered},
};
use thing_runtime::{MediaAction, MediaSnapshot};
use zbus::{
    Connection, MatchRule, MessageStream,
    fdo::DBusProxy,
    message::{Flags, Message, Type},
    names::BusName,
    zvariant::OwnedValue,
};

use model::{Model, PlaybackMethod, Player, Target};

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT_INTERFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const MAX_PLAYERS: usize = 64;
const MAX_COMMANDS: usize = 128;
const MAX_PLAYER_QUERIES: usize = 4;
const MAX_PROPERTY_BYTES: usize = 256 * 1024;
const METHOD_TIMEOUT: Duration = Duration::from_secs(2);
const QUERY_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct Command {
    target: Target,
    method: PlaybackMethod,
}

#[derive(Clone)]
struct KnownPlayer {
    owner: String,
    id: String,
}

#[derive(Clone)]
struct QueryRequest {
    name: String,
    player: KnownPlayer,
    tracks_transition: bool,
}

struct QueryResult {
    request: QueryRequest,
    result: Result<Player, String>,
}

struct DelayedQuery {
    deadline: Instant,
    request: QueryRequest,
}

type PlayerQuery = LocalBoxFuture<'static, QueryResult>;

#[derive(Default)]
struct Shared {
    commands: VecDeque<Command>,
    current: Option<Target>,
    targets: BTreeMap<String, Target>,
    online: bool,
    stop: bool,
    latest: Option<MediaSnapshot>,
}

pub(crate) struct Media {
    shared: Arc<Mutex<Shared>>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl Media {
    pub fn start(ping: Ping) -> io::Result<Self> {
        Self::start_at(None, ping)
    }

    fn start_at(address: Option<String>, ping: Ping) -> io::Result<Self> {
        let (wake, reader) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("thing-media".into())
            .spawn(move || {
                let mut previous = MediaSnapshot::default();
                let mut last_error = String::new();
                let mut next_player_id = 1_u64;
                loop {
                    if lock(&state).stop {
                        break;
                    }
                    let result = async_io::block_on(session(
                        address.as_deref(),
                        &state,
                        &reader,
                        &mut next_player_id,
                        |snapshot| publish(&state, &ping, &mut previous, snapshot),
                    ));
                    {
                        let mut shared = lock(&state);
                        shared.online = false;
                        shared.current = None;
                        shared.targets.clear();
                        shared.commands.clear();
                        if shared.stop {
                            break;
                        }
                    }
                    let error = result
                        .err()
                        .unwrap_or_else(|| "media connection ended".into());
                    if error != last_error {
                        eprintln!("thing: media unavailable: {error}; retrying in 1 second");
                        last_error = error.clone();
                    }
                    publish(
                        &state,
                        &ping,
                        &mut previous,
                        MediaSnapshot {
                            error: sanitize(error, 4096),
                            ..MediaSnapshot::default()
                        },
                    );
                    retry_wait(&reader, Duration::from_secs(1));
                }
            })?;
        Ok(Self {
            shared,
            wake,
            worker: Some(worker),
        })
    }

    pub fn take_snapshot(&self) -> Option<MediaSnapshot> {
        lock(&self.shared).latest.take()
    }

    pub fn send(&self, action: MediaAction, player: Option<&str>) -> Result<(), &'static str> {
        let mut shared = lock(&self.shared);
        if !shared.online {
            return Err("media service is disconnected");
        }
        let target = match player {
            Some(id) => shared
                .targets
                .get(id)
                .cloned()
                .ok_or("media player is no longer available")?,
            None => shared
                .current
                .clone()
                .ok_or("no media player is available")?,
        };
        let Some(method) = target.method(action) else {
            return Err(match player {
                Some(_) => "that media player does not support the action",
                None => "the selected media player does not support the action",
            });
        };
        if shared.commands.len() >= MAX_COMMANDS {
            return Err("media command queue is full");
        }
        shared.commands.push_back(Command { target, method });
        drop(shared);
        wake(&self.wake);
        Ok(())
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        lock(&self.shared).stop = true;
        wake(&self.wake);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn publish(
    shared: &Mutex<Shared>,
    ping: &Ping,
    previous: &mut MediaSnapshot,
    snapshot: MediaSnapshot,
) {
    if *previous != snapshot {
        *previous = snapshot.clone();
        lock(shared).latest = Some(snapshot);
        ping.ping();
    }
}

async fn session(
    address: Option<&str>,
    shared: &Mutex<Shared>,
    wake_stream: &UnixStream,
    next_player_id: &mut u64,
    mut publish: impl FnMut(MediaSnapshot),
) -> Result<(), String> {
    let reader = wake_stream.try_clone().map_err(|error| error.to_string())?;
    let wake_source = Async::new(reader).map_err(|error| error.to_string())?;
    let builder = match address {
        Some(address) => zbus::connection::Builder::address(address),
        None => zbus::connection::Builder::session(),
    }
    .map_err(|error| error.to_string())?;
    let connection = future::race(
        async {
            builder
                .method_timeout(METHOD_TIMEOUT)
                .build()
                .await
                .map(Some)
                .map_err(|error| error.to_string())
        },
        future::race(
            async {
                wake_source
                    .readable()
                    .await
                    .map_err(|error| error.to_string())?;
                drain(wake_source.get_ref());
                Ok::<Option<Connection>, String>(None)
            },
            async {
                Timer::after(METHOD_TIMEOUT).await;
                Err::<Option<Connection>, String>("media connection timed out".into())
            },
        ),
    )
    .await?;
    let Some(connection) = connection else {
        return if stopping(shared) {
            Ok(())
        } else {
            Err("media connection interrupted".into())
        };
    };
    if stopping(shared) {
        return Ok(());
    }
    let name_rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.DBus")
        .map_err(|error| error.to_string())?
        .interface("org.freedesktop.DBus")
        .map_err(|error| error.to_string())?
        .member("NameOwnerChanged")
        .map_err(|error| error.to_string())?
        .arg0ns("org.mpris.MediaPlayer2")
        .map_err(|error| error.to_string())?
        .build();
    let property_rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .path(MPRIS_PATH)
        .map_err(|error| error.to_string())?
        .interface(PROPERTIES_INTERFACE)
        .map_err(|error| error.to_string())?
        .member("PropertiesChanged")
        .map_err(|error| error.to_string())?
        .add_arg(PLAYER_INTERFACE)
        .map_err(|error| error.to_string())?
        .build();
    // install both matches before enumeration so arrivals and replacements
    // during the initial queries remain queued for the event loop.
    let mut names = MessageStream::for_match_rule(name_rule, &connection, Some(256))
        .await
        .map_err(|error| error.to_string())?;
    if stopping(shared) {
        return Ok(());
    }
    let mut properties = MessageStream::for_match_rule(property_rule, &connection, Some(256))
        .await
        .map_err(|error| error.to_string())?;
    if stopping(shared) {
        return Ok(());
    }
    let dbus = DBusProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())?;
    if stopping(shared) {
        return Ok(());
    }
    let listed = dbus.list_names().await.map_err(|error| error.to_string())?;
    if stopping(shared) {
        return Ok(());
    }
    let candidates = listed
        .into_iter()
        .map(|name| name.to_string())
        .filter(|name| is_player_name(name))
        .take(MAX_PLAYERS);
    let mut owners = stream::iter(candidates)
        .map(|name| {
            let dbus = &dbus;
            async move {
                let bus_name =
                    BusName::try_from(name.as_str()).map_err(|error| error.to_string())?;
                let owner = dbus
                    .get_name_owner(bus_name)
                    .await
                    .map_err(|error| error.to_string())?
                    .to_string();
                Ok::<_, String>((name, owner))
            }
        })
        .buffer_unordered(MAX_PLAYER_QUERIES);
    let mut known = BTreeMap::new();
    let mut queued = BTreeMap::new();
    while let Some(result) = owners.next().await {
        if stopping(shared) {
            return Ok(());
        }
        let Ok((name, owner)) = result else {
            continue;
        };
        if let Some(player) = register_player(&mut known, &name, &owner, next_player_id)? {
            queue_player_query(&mut queued, name, player, false);
        }
    }
    let mut model = Model::default();
    publish(update_shared(shared, &model));
    let mut running = BTreeSet::new();
    let mut queries = FuturesUnordered::<PlayerQuery>::new();
    let mut retries = BTreeMap::new();

    loop {
        if stopping(shared) {
            return Ok(());
        }
        start_player_queries(&connection, &mut queued, &mut running, &mut queries);
        if drain_pending_name_events(
            &mut names,
            &mut known,
            &mut queued,
            &mut retries,
            &mut model,
            next_player_id,
        )? {
            publish(update_shared(shared, &model));
        }
        if stopping(shared) {
            return Ok(());
        }
        let retry_deadline = retries.values().map(|retry| retry.deadline).min();
        let input = future::race(
            async { Input::Name(names.next().await) },
            future::race(
                async { Input::Property(properties.next().await) },
                future::race(
                    async {
                        let Some(result) = queries.next().await else {
                            return future::pending().await;
                        };
                        Input::Query(result)
                    },
                    future::race(
                        async move {
                            let Some(deadline) = retry_deadline else {
                                return future::pending().await;
                            };
                            Timer::at(deadline).await;
                            Input::Retry
                        },
                        future::race(
                            async {
                                let command = { lock(shared).commands.pop_front() };
                                let Some(command) = command else {
                                    return future::pending().await;
                                };
                                Input::Command(command)
                            },
                            async { Input::Wake(wake_source.readable().await) },
                        ),
                    ),
                ),
            ),
        )
        .await;
        match input {
            Input::Wake(result) => {
                result.map_err(|error| error.to_string())?;
                drain(wake_source.get_ref());
            }
            Input::Name(Some(Ok(message))) => {
                if apply_name_event(
                    message,
                    &mut known,
                    &mut queued,
                    &mut retries,
                    &mut model,
                    next_player_id,
                )? {
                    publish(update_shared(shared, &model));
                }
            }
            Input::Name(Some(Err(error))) => return Err(error.to_string()),
            Input::Name(None) => return Err("media name subscription ended".into()),
            Input::Property(Some(Ok(message))) => {
                let Some(owner) = message.header().sender().map(ToString::to_string) else {
                    continue;
                };
                for (name, player) in known.iter().filter(|(_, player)| player.owner == owner) {
                    retries.remove(&player.id);
                    queue_player_query(&mut queued, name.clone(), player.clone(), true);
                }
            }
            Input::Property(Some(Err(error))) => return Err(error.to_string()),
            Input::Property(None) => return Err("media property subscription ended".into()),
            Input::Command(command) => {
                let current = model.player(&command.target.name).is_some_and(|player| {
                    player.id == command.target.id && player.owner == command.target.owner
                });
                if current
                    && !send_action_interruptible(&connection, command, shared, &wake_source)
                        .await?
                {
                    return Ok(());
                }
            }
            Input::Query(QueryResult { request, result }) => {
                running.remove(&request.player.id);
                if drain_pending_name_events(
                    &mut names,
                    &mut known,
                    &mut queued,
                    &mut retries,
                    &mut model,
                    next_player_id,
                )? {
                    publish(update_shared(shared, &model));
                }
                let current = known.get(&request.name).is_some_and(|player| {
                    player.id == request.player.id && player.owner == request.player.owner
                });
                let discovered = model.player(&request.name).is_some_and(|player| {
                    player.id == request.player.id && player.owner == request.player.owner
                });
                if current {
                    match result {
                        Ok(player) => {
                            retries.remove(&request.player.id);
                            if request.tracks_transition {
                                model.update(player);
                            } else {
                                model.discover(player);
                            }
                            publish(update_shared(shared, &model));
                        }
                        Err(_) if !discovered => schedule_player_retry(&mut retries, request),
                        Err(_) => {}
                    }
                }
            }
            Input::Retry => {
                let Some(request) = take_due_player_retry(&mut retries, Instant::now()) else {
                    continue;
                };
                if drain_pending_name_events(
                    &mut names,
                    &mut known,
                    &mut queued,
                    &mut retries,
                    &mut model,
                    next_player_id,
                )? {
                    publish(update_shared(shared, &model));
                }
                let current = known.get(&request.name).is_some_and(|player| {
                    player.id == request.player.id && player.owner == request.player.owner
                });
                let discovered = model.player(&request.name).is_some_and(|player| {
                    player.id == request.player.id && player.owner == request.player.owner
                });
                if current
                    && !discovered
                    && !running.contains(&request.player.id)
                    && !queued.contains_key(&request.player.id)
                {
                    queue_player_query(
                        &mut queued,
                        request.name,
                        request.player,
                        request.tracks_transition,
                    );
                }
            }
        }
    }
}

enum Input {
    Name(Option<zbus::Result<Message>>),
    Property(Option<zbus::Result<Message>>),
    Wake(io::Result<()>),
    Command(Command),
    Query(QueryResult),
    Retry,
}

fn schedule_player_retry(retries: &mut BTreeMap<String, DelayedQuery>, request: QueryRequest) {
    if retries.contains_key(&request.player.id) || retries.len() >= MAX_PLAYERS {
        return;
    }
    retries.insert(
        request.player.id.clone(),
        DelayedQuery {
            deadline: Instant::now() + QUERY_RETRY_DELAY,
            request,
        },
    );
}

fn take_due_player_retry(
    retries: &mut BTreeMap<String, DelayedQuery>,
    now: Instant,
) -> Option<QueryRequest> {
    let id = retries
        .iter()
        .find(|(_, retry)| retry.deadline <= now)
        .map(|(id, _)| id.clone())?;
    retries.remove(&id).map(|retry| retry.request)
}

fn discard_player_queries(
    queued: &mut BTreeMap<String, QueryRequest>,
    retries: &mut BTreeMap<String, DelayedQuery>,
    id: &str,
) {
    queued.remove(id);
    retries.remove(id);
}

fn queue_player_query(
    queued: &mut BTreeMap<String, QueryRequest>,
    name: String,
    player: KnownPlayer,
    tracks_transition: bool,
) {
    if queued.contains_key(&player.id) || queued.len() < MAX_PLAYERS {
        queued.insert(
            player.id.clone(),
            QueryRequest {
                name,
                player,
                tracks_transition,
            },
        );
    }
}

fn start_player_queries(
    connection: &Connection,
    queued: &mut BTreeMap<String, QueryRequest>,
    running: &mut BTreeSet<String>,
    queries: &mut FuturesUnordered<PlayerQuery>,
) {
    while running.len() < MAX_PLAYER_QUERIES {
        let Some(id) = queued.keys().find(|id| !running.contains(*id)).cloned() else {
            break;
        };
        let request = queued.remove(&id).expect("queued media query disappeared");
        running.insert(id);
        let connection = connection.clone();
        queries.push(
            async move {
                let result = query_player(
                    &connection,
                    &request.player.id,
                    &request.name,
                    &request.player.owner,
                )
                .await;
                QueryResult { request, result }
            }
            .boxed_local(),
        );
    }
}

fn drain_pending_name_events(
    names: &mut MessageStream,
    known: &mut BTreeMap<String, KnownPlayer>,
    queued: &mut BTreeMap<String, QueryRequest>,
    retries: &mut BTreeMap<String, DelayedQuery>,
    model: &mut Model,
    next_player_id: &mut u64,
) -> Result<bool, String> {
    let mut changed = false;
    loop {
        match names.next().now_or_never() {
            None => return Ok(changed),
            Some(Some(Ok(message))) => {
                changed |=
                    apply_name_event(message, known, queued, retries, model, next_player_id)?;
            }
            Some(Some(Err(error))) => return Err(error.to_string()),
            Some(None) => return Err("media name subscription ended".into()),
        }
    }
}

fn apply_name_event(
    message: Message,
    known: &mut BTreeMap<String, KnownPlayer>,
    queued: &mut BTreeMap<String, QueryRequest>,
    retries: &mut BTreeMap<String, DelayedQuery>,
    model: &mut Model,
    next_player_id: &mut u64,
) -> Result<bool, String> {
    let Ok((name, old_owner, new_owner)) = message.body().deserialize::<(String, String, String)>()
    else {
        return Ok(false);
    };
    if !is_player_name(&name) {
        return Ok(false);
    }

    let mut changed = false;
    if !old_owner.is_empty() {
        model.remove(&name, &old_owner);
        if let Some(player) = known
            .get(&name)
            .filter(|player| player.owner == old_owner)
            .cloned()
        {
            known.remove(&name);
            discard_player_queries(queued, retries, &player.id);
        }
        changed = true;
    }
    if !new_owner.is_empty() {
        if let Some(player) = known
            .get(&name)
            .filter(|player| player.owner != new_owner)
            .cloned()
        {
            model.remove(&name, &player.owner);
            known.remove(&name);
            discard_player_queries(queued, retries, &player.id);
            changed = true;
        }
        if let Some(player) = register_player(known, &name, &new_owner, next_player_id)? {
            queue_player_query(queued, name, player, true);
            changed = true;
        }
    }
    Ok(changed)
}

fn update_shared(shared: &Mutex<Shared>, model: &Model) -> MediaSnapshot {
    let projection = model.snapshot();
    let mut shared = lock(shared);
    shared.current = projection.selected;
    shared.targets = projection.targets;
    shared.online = true;
    projection.snapshot
}

async fn query_player(
    connection: &Connection,
    id: &str,
    name: &str,
    owner: &str,
) -> Result<Player, String> {
    let (root, player) = future::zip(
        get_all(connection, owner, ROOT_INTERFACE),
        get_all(connection, owner, PLAYER_INTERFACE),
    )
    .await;
    let mut root = root.unwrap_or_default();
    let mut player = player.map_err(|error| error.to_string())?;
    let mut metadata =
        take::<HashMap<String, OwnedValue>>(&mut player, "Metadata").unwrap_or_default();
    let status = take::<String>(&mut player, "PlaybackStatus").unwrap_or_default();
    Ok(Player {
        id: id.into(),
        name: name.into(),
        owner: owner.into(),
        playing: status == "Playing",
        title: take_string(&mut metadata, "xesam:title", 4096),
        artist: take_artists(&mut metadata),
        art_url: take_string(&mut metadata, "mpris:artUrl", 16 * 1024),
        identity: take_string(&mut root, "Identity", 4096),
        desktop_entry: take_string(&mut root, "DesktopEntry", 4096),
        can_play: take::<bool>(&mut player, "CanPlay").unwrap_or(false),
        can_pause: take::<bool>(&mut player, "CanPause").unwrap_or(false),
        can_next: take::<bool>(&mut player, "CanGoNext").unwrap_or(false),
        can_previous: take::<bool>(&mut player, "CanGoPrevious").unwrap_or(false),
        can_control: take::<bool>(&mut player, "CanControl").unwrap_or(false),
    })
}

fn allocate_player_id(next: &mut u64) -> Result<String, String> {
    let id = *next;
    *next = next
        .checked_add(1)
        .ok_or_else(|| "media player id space exhausted".to_owned())?;
    Ok(format!("p{id}"))
}

fn register_player(
    known: &mut BTreeMap<String, KnownPlayer>,
    name: &str,
    owner: &str,
    next_player_id: &mut u64,
) -> Result<Option<KnownPlayer>, String> {
    if let Some(player) = known.get(name).filter(|player| player.owner == owner) {
        return Ok(Some(player.clone()));
    }
    if !known.contains_key(name) && known.len() >= MAX_PLAYERS {
        return Ok(None);
    }
    let player = KnownPlayer {
        owner: owner.into(),
        id: allocate_player_id(next_player_id)?,
    };
    known.insert(name.into(), player.clone());
    Ok(Some(player))
}

async fn get_all(
    connection: &Connection,
    owner: &str,
    interface: &str,
) -> zbus::Result<HashMap<String, OwnedValue>> {
    let message = connection
        .call_method(
            Some(owner),
            MPRIS_PATH,
            Some(PROPERTIES_INTERFACE),
            "GetAll",
            &(interface,),
        )
        .await?;
    let body = message.body();
    if body.len() > MAX_PROPERTY_BYTES {
        return Err(zbus::Error::ExcessData);
    }
    body.deserialize()
}

async fn send_action(connection: &Connection, command: Command) -> Result<(), String> {
    let method = match command.method {
        PlaybackMethod::Play => "Play",
        PlaybackMethod::Pause => "Pause",
        PlaybackMethod::Next => "Next",
        PlaybackMethod::Previous => "Previous",
        PlaybackMethod::Stop => "Stop",
    };
    let message = Message::method_call(MPRIS_PATH, method)
        .and_then(|builder| builder.destination(command.target.owner.as_str()))
        .and_then(|builder| builder.interface(PLAYER_INTERFACE))
        .and_then(|builder| builder.with_flags(Flags::NoReplyExpected))
        .and_then(|builder| builder.with_flags(Flags::NoAutoStart))
        .and_then(|builder| builder.build(&()))
        .map_err(|error| error.to_string())?;
    // a d-bus frame can require several socket writes. the caller either lets
    // this finish or abandons the entire connection.
    connection
        .send(&message)
        .await
        .map_err(|error| error.to_string())
}

async fn send_action_interruptible(
    connection: &Connection,
    command: Command,
    shared: &Mutex<Shared>,
    wake_source: &Async<UnixStream>,
) -> Result<bool, String> {
    enum Progress {
        Sent(Result<(), String>),
        Wake(io::Result<()>),
        Timeout,
    }

    let send = send_action(connection, command);
    let timeout = Timer::after(METHOD_TIMEOUT);
    futures_lite::pin!(send);
    futures_lite::pin!(timeout);
    loop {
        let progress = future::race(
            async { Progress::Sent(send.as_mut().await) },
            future::race(
                async { Progress::Wake(wake_source.readable().await) },
                async {
                    timeout.as_mut().await;
                    Progress::Timeout
                },
            ),
        )
        .await;
        match progress {
            Progress::Sent(result) => return result.map(|()| true),
            Progress::Wake(result) => {
                result.map_err(|error| error.to_string())?;
                drain(wake_source.get_ref());
                if stopping(shared) {
                    // the connection is dropped immediately with this send. it must not be
                    // reused because cancellation may leave a partial d-bus frame.
                    return Ok(false);
                }
            }
            // returning ends the session and drops the entire connection with the send.
            Progress::Timeout => return Err("media command timed out".into()),
        }
    }
}

fn take<T>(values: &mut HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    values.remove(key).and_then(|value| value.try_into().ok())
}

fn take_string(values: &mut HashMap<String, OwnedValue>, key: &str, max: usize) -> String {
    sanitize(take::<String>(values, key).unwrap_or_default(), max)
}

fn take_artists(values: &mut HashMap<String, OwnedValue>) -> String {
    let artists = take::<Vec<String>>(values, "xesam:artist").unwrap_or_default();
    sanitize(
        artists.into_iter().take(16).collect::<Vec<_>>().join(", "),
        4096,
    )
}

fn sanitize(mut value: String, max: usize) -> String {
    value.retain(|character| character != '\0');
    if value.len() > max {
        let mut boundary = max;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
    }
    value
}

fn is_player_name(name: &str) -> bool {
    name.strip_prefix(MPRIS_PREFIX)
        .is_some_and(|suffix| !suffix.is_empty())
}

fn wake(mut stream: &UnixStream) {
    loop {
        match stream.write(&[1]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            _ => break,
        }
    }
}

fn drain(mut stream: &UnixStream) {
    let mut bytes = [0; 256];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn retry_wait(stream: &UnixStream, timeout: Duration) {
    let mut descriptor = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let milliseconds = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
    loop {
        // SAFETY: the descriptor points to one initialized pollfd for this call.
        let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
        if result >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break;
        }
    }
    drain(stream);
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn stopping(shared: &Mutex<Shared>) -> bool {
    lock(shared).stop
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader},
        os::unix::net::UnixListener,
        process::{Child, Command as ProcessCommand, Stdio},
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
    };

    use tempfile::TempDir;
    use zbus::zvariant::Value;

    use super::*;

    const PLAYER_NAME: &str = "org.mpris.MediaPlayer2.thing-test";
    const SECOND_PLAYER_NAME: &str = "org.mpris.MediaPlayer2.thing-test-second";

    #[test]
    fn player_names_require_an_instance_suffix() {
        assert!(is_player_name("org.mpris.MediaPlayer2.firefox"));
        assert!(!is_player_name("org.mpris.MediaPlayer2"));
        assert!(!is_player_name("org.mpris.MediaPlayer20.firefox"));
    }

    #[test]
    fn failed_query_retries_are_bounded_and_deduplicated() {
        let mut retries = BTreeMap::new();
        for index in 0..=MAX_PLAYERS {
            schedule_player_retry(
                &mut retries,
                QueryRequest {
                    name: format!("{PLAYER_NAME}-{index}"),
                    player: KnownPlayer {
                        owner: format!(":1.{index}"),
                        id: format!("p{index}"),
                    },
                    tracks_transition: false,
                },
            );
        }
        assert_eq!(retries.len(), MAX_PLAYERS);

        let first_deadline = retries["p0"].deadline;
        schedule_player_retry(
            &mut retries,
            QueryRequest {
                name: PLAYER_NAME.into(),
                player: KnownPlayer {
                    owner: ":1.0".into(),
                    id: "p0".into(),
                },
                tracks_transition: true,
            },
        );
        assert_eq!(retries.len(), MAX_PLAYERS);
        assert_eq!(retries["p0"].deadline, first_deadline);
        assert!(!retries["p0"].request.tracks_transition);

        let mut queued = BTreeMap::from([("p1".into(), retries["p1"].request.clone())]);
        discard_player_queries(&mut queued, &mut retries, "p1");
        assert!(!queued.contains_key("p1"));
        assert!(!retries.contains_key("p1"));

        retries.get_mut("p0").unwrap().deadline = Instant::now();
        assert_eq!(
            take_due_player_retry(&mut retries, Instant::now())
                .unwrap()
                .player
                .id,
            "p0"
        );
    }

    #[test]
    fn strings_are_bounded_on_utf8_boundaries() {
        assert_eq!(sanitize("a\0b".into(), 8), "ab");
        assert_eq!(sanitize("ééé".into(), 5), "éé");
    }

    #[test]
    fn duplicate_acquisition_keeps_the_current_lifetime_id() {
        let mut known = BTreeMap::new();
        let mut next = 1;
        let first = register_player(&mut known, PLAYER_NAME, ":1.7", &mut next)
            .unwrap()
            .unwrap();
        let duplicate = register_player(&mut known, PLAYER_NAME, ":1.7", &mut next)
            .unwrap()
            .unwrap();
        assert_eq!(duplicate.id, first.id);

        known.remove(PLAYER_NAME);
        let reacquired = register_player(&mut known, PLAYER_NAME, ":1.7", &mut next)
            .unwrap()
            .unwrap();
        assert_ne!(reacquired.id, first.id);
    }

    #[test]
    fn queued_action_keeps_the_owner_selected_at_dispatch() {
        let (wake, _reader) = UnixStream::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        let first = target("p1", PLAYER_NAME, ":1.7");
        let shared = Arc::new(Mutex::new(Shared {
            current: Some(first.clone()),
            targets: BTreeMap::from([(first.id.clone(), first)]),
            online: true,
            ..Shared::default()
        }));
        let media = Media {
            shared: shared.clone(),
            wake,
            worker: None,
        };

        media.send(MediaAction::PlayPause, None).unwrap();
        lock(&shared).current = Some(target("p2", PLAYER_NAME, ":1.8"));
        assert_eq!(lock(&shared).commands[0].target.owner, ":1.7");
        assert_eq!(lock(&shared).commands[0].method, PlaybackMethod::Pause);
    }

    #[test]
    fn explicit_action_targets_a_player_without_changing_selection() {
        let (wake, _reader) = UnixStream::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        let selected = target("p1", "org.mpris.MediaPlayer2.first", ":1.7");
        let other = target("p2", "org.mpris.MediaPlayer2.second", ":1.8");
        let shared = Arc::new(Mutex::new(Shared {
            current: Some(selected.clone()),
            targets: BTreeMap::from([
                (selected.id.clone(), selected.clone()),
                (other.id.clone(), other.clone()),
            ]),
            online: true,
            ..Shared::default()
        }));
        let media = Media {
            shared: shared.clone(),
            wake,
            worker: None,
        };

        media.send(MediaAction::Next, Some("p2")).unwrap();
        let shared = lock(&shared);
        assert_eq!(shared.commands[0].target, other);
        assert_eq!(shared.current, Some(selected));
        drop(shared);
        assert_eq!(
            media.send(MediaAction::Next, Some("retired")),
            Err("media player is no longer available")
        );
    }

    #[test]
    fn private_bus_covers_lifecycle_actions_and_reconnect() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let first_state = Arc::new(Mutex::new(FakeState::new("first title", true)));
        let first_calls = Arc::new(FakeCalls::default());
        let first = fake_player(&address, first_state.clone(), first_calls.clone());
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(address.clone()), ping).unwrap();

        let snapshot = wait_for(&media, |snapshot| snapshot.available);
        let first_id = snapshot.selected_id.clone();
        let first_owner = first.unique_name().unwrap().to_string();
        assert_eq!(snapshot.players.len(), 1);
        assert_eq!(snapshot.players[0].id, first_id);
        assert_eq!(snapshot.title, "first title");
        assert_eq!(snapshot.artist, "test artist");
        assert_eq!(snapshot.art_url, "file:///tmp/test-cover.png");
        assert_eq!(snapshot.identity, "thing test player");
        assert_eq!(snapshot.desktop_entry, "thing-test");
        assert!(snapshot.playing);
        assert!(snapshot.can_play_pause);
        media.send(MediaAction::PlayPause, None).unwrap();
        wait_until(|| first_calls.pause.load(Ordering::Acquire) == 1);
        assert_eq!(first_calls.play.load(Ordering::Acquire), 0);
        assert_eq!(first_calls.play_pause.load(Ordering::Acquire), 0);

        assert!(async_io::block_on(first.release_name(PLAYER_NAME)).unwrap());
        wait_for(&media, |snapshot| !snapshot.available);
        assert_eq!(
            media.send(MediaAction::Next, Some(&first_id)),
            Err("media player is no longer available")
        );
        async_io::block_on(first.request_name(PLAYER_NAME)).unwrap();
        let snapshot = wait_for(&media, |snapshot| {
            snapshot.available && snapshot.selected_id != first_id
        });
        let reacquired_id = snapshot.selected_id.clone();
        assert_eq!(first.unique_name().unwrap().to_string(), first_owner);
        assert_eq!(
            media.send(MediaAction::Next, Some(&first_id)),
            Err("media player is no longer available")
        );

        {
            let mut state = first_state.lock().unwrap();
            state.title = "changed title".into();
            state.playing = false;
            state.can_play = true;
            state.can_pause = false;
        }
        emit_invalidated_properties(&first);
        let snapshot = wait_for(&media, |snapshot| snapshot.title == "changed title");
        assert_eq!(snapshot.selected_id, reacquired_id);
        assert!(!snapshot.playing);
        first_state.lock().unwrap().title = "after malformed signal".into();
        emit_malformed_properties(&first);
        wait_for(&media, |snapshot| {
            snapshot.title == "after malformed signal"
        });
        media.send(MediaAction::PlayPause, None).unwrap();
        wait_until(|| first_calls.play.load(Ordering::Acquire) == 1);
        assert_eq!(first_calls.pause.load(Ordering::Acquire), 1);
        assert_eq!(first_calls.play_pause.load(Ordering::Acquire), 0);

        drop(first);
        let snapshot = wait_for(&media, |snapshot| !snapshot.available);
        assert!(snapshot.error.is_empty());
        let second_state = Arc::new(Mutex::new(FakeState::new("second title", false)));
        let second_calls = Arc::new(FakeCalls::default());
        let second = fake_player(&address, second_state, second_calls.clone());
        let snapshot = wait_for(&media, |snapshot| snapshot.title == "second title");
        assert!(snapshot.available);
        let second_id = snapshot.selected_id.clone();
        media.send(MediaAction::PlayPause, None).unwrap();
        wait_until(|| second_calls.play.load(Ordering::Acquire) == 1);
        assert_eq!(second_calls.pause.load(Ordering::Acquire), 0);
        assert_eq!(second_calls.play_pause.load(Ordering::Acquire), 0);
        assert_eq!(first_calls.play.load(Ordering::Acquire), 1);
        assert_eq!(first_calls.pause.load(Ordering::Acquire), 1);

        drop(second);
        bus.stop();
        let snapshot = wait_for(&media, |snapshot| {
            !snapshot.available && !snapshot.error.is_empty()
        });
        assert!(!snapshot.error.is_empty());
        bus = Bus::start(&address);
        let third = fake_player(
            &address,
            Arc::new(Mutex::new(FakeState::new("after reconnect", true))),
            Arc::new(FakeCalls::default()),
        );
        let snapshot = wait_for(&media, |snapshot| snapshot.title == "after reconnect");
        assert!(snapshot.available);
        assert!(snapshot.error.is_empty());
        assert_eq!(third.unique_name().unwrap().to_string(), first_owner);
        assert_ne!(snapshot.selected_id, first_id);
        assert_ne!(snapshot.selected_id, reacquired_id);
        assert_ne!(snapshot.selected_id, second_id);
        assert_eq!(
            media.send(MediaAction::Next, Some(&first_id)),
            Err("media player is no longer available")
        );

        drop(third);
        drop(media);
        bus.stop();
    }

    #[test]
    fn private_bus_routes_an_explicit_action_to_the_requested_player() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let selected_calls = Arc::new(FakeCalls::default());
        let selected = fake_player_named(
            &address,
            PLAYER_NAME,
            Arc::new(Mutex::new(FakeState::new("selected", true))),
            selected_calls.clone(),
        );
        let other_calls = Arc::new(FakeCalls::default());
        let other = fake_player_named(
            &address,
            SECOND_PLAYER_NAME,
            Arc::new(Mutex::new(FakeState::new("other", false))),
            other_calls.clone(),
        );
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(address), ping).unwrap();

        let snapshot = wait_for(&media, |snapshot| snapshot.players.len() == 2);
        let selected_id = snapshot.selected_id;
        let other_id = snapshot
            .players
            .iter()
            .find(|player| player.name == SECOND_PLAYER_NAME)
            .unwrap()
            .id
            .clone();
        media.send(MediaAction::PlayPause, Some(&other_id)).unwrap();
        wait_until(|| other_calls.play.load(Ordering::Acquire) == 1);
        assert_eq!(selected_calls.play.load(Ordering::Acquire), 0);
        assert_eq!(selected_calls.pause.load(Ordering::Acquire), 0);
        assert_eq!(
            lock(&media.shared).current.as_ref().unwrap().id,
            selected_id
        );

        drop(media);
        drop(other);
        drop(selected);
        bus.stop();
    }

    #[test]
    fn one_wake_drains_multiple_queued_commands() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let calls = Arc::new(FakeCalls::default());
        let player = fake_player(
            &address,
            Arc::new(Mutex::new(FakeState::new("playing", true))),
            calls.clone(),
        );
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(address), ping).unwrap();
        wait_for(&media, |snapshot| snapshot.available);

        let target = lock(&media.shared).current.clone().unwrap();
        lock(&media.shared).commands.extend([
            Command {
                target: target.clone(),
                method: PlaybackMethod::Pause,
            },
            Command {
                target,
                method: PlaybackMethod::Pause,
            },
        ]);
        wake(&media.wake);
        wait_until(|| calls.pause.load(Ordering::Acquire) == 2);

        drop(media);
        drop(player);
        bus.stop();
    }

    #[test]
    fn stale_action_does_not_cross_same_owner_reacquisition_while_query_is_stalled() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let state = Arc::new(Mutex::new(FakeState::new("first", true)));
        let calls = Arc::new(FakeCalls::default());
        let player = fake_player(&address, state.clone(), calls.clone());
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(address), ping).unwrap();

        let snapshot = wait_for(&media, |snapshot| snapshot.available);
        let retired_id = snapshot.selected_id;
        let query_count = calls.metadata_queries.load(Ordering::Acquire);
        state.lock().unwrap().stall_metadata = true;
        emit_invalidated_properties(&player);
        wait_until(|| calls.metadata_queries.load(Ordering::Acquire) > query_count);

        assert!(async_io::block_on(player.release_name(PLAYER_NAME)).unwrap());
        async_io::block_on(player.request_name(PLAYER_NAME)).unwrap();
        let action = media.send(MediaAction::PlayPause, Some(&retired_id));
        assert!(
            action.is_ok() || action == Err("media player is no longer available"),
            "unexpected action result: {action:?}"
        );
        state.lock().unwrap().stall_metadata = false;

        wait_for(&media, |snapshot| {
            snapshot.available && snapshot.selected_id != retired_id
        });
        assert_eq!(calls.pause.load(Ordering::Acquire), 0);

        drop(media);
        drop(player);
        bus.stop();
    }

    #[test]
    fn stale_query_result_cannot_restore_a_retired_lifetime() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let state = Arc::new(Mutex::new(FakeState::new("first", true)));
        let calls = Arc::new(FakeCalls::default());
        let player = fake_player(&address, state.clone(), calls.clone());
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(address), ping).unwrap();

        let retired_id = wait_for(&media, |snapshot| snapshot.available).selected_id;
        let query_count = calls.metadata_queries.load(Ordering::Acquire);
        {
            let mut state = state.lock().unwrap();
            state.metadata_delay = Duration::from_millis(300);
            state.title = "delayed".into();
        }
        emit_invalidated_properties(&player);
        wait_until(|| calls.metadata_queries.load(Ordering::Acquire) > query_count);
        {
            let mut state = state.lock().unwrap();
            state.metadata_delay = Duration::ZERO;
            state.title = "replacement".into();
        }
        assert!(async_io::block_on(player.release_name(PLAYER_NAME)).unwrap());
        async_io::block_on(player.request_name(PLAYER_NAME)).unwrap();

        let snapshot = wait_for(&media, |snapshot| {
            snapshot.title == "replacement" && snapshot.selected_id != retired_id
        });
        let replacement_id = snapshot.selected_id;
        thread::sleep(Duration::from_millis(400));
        assert_eq!(
            lock(&media.shared).current.as_ref().unwrap().id,
            replacement_id
        );
        assert_eq!(
            media.send(MediaAction::Next, Some(&retired_id)),
            Err("media player is no longer available")
        );

        drop(media);
        drop(player);
        bus.stop();
    }

    #[test]
    fn oversized_properties_are_rejected_before_decoding() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let state = Arc::new(Mutex::new(FakeState::new(
            &"x".repeat(MAX_PROPERTY_BYTES + 1),
            false,
        )));
        let player = fake_player(&address, state.clone(), Arc::new(FakeCalls::default()));
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(address), ping).unwrap();

        wait_until(|| lock(&media.shared).online);
        assert!(lock(&media.shared).current.is_none());
        state.lock().unwrap().title = "recovered title".into();
        emit_invalidated_properties(&player);
        let snapshot = wait_for(&media, |snapshot| snapshot.title == "recovered title");
        assert!(snapshot.available);

        drop(media);
        drop(player);
        bus.stop();
    }

    #[test]
    fn initial_query_failure_recovers_without_another_signal() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let state = Arc::new(Mutex::new(FakeState::new(
            &"x".repeat(MAX_PROPERTY_BYTES + 1),
            true,
        )));
        let calls = Arc::new(FakeCalls::default());
        let player = fake_player(&address, state.clone(), calls.clone());
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(address), ping).unwrap();

        wait_until(|| calls.metadata_queries.load(Ordering::Acquire) == 1);
        state.lock().unwrap().title = "recovered title".into();
        let snapshot = wait_for(&media, |snapshot| snapshot.title == "recovered title");
        assert!(snapshot.available);
        assert!(calls.metadata_queries.load(Ordering::Acquire) >= 2);

        drop(media);
        drop(player);
        bus.stop();
    }

    #[test]
    fn property_signal_supersedes_a_pending_discovery_retry() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let state = Arc::new(Mutex::new(FakeState::new(
            &"x".repeat(MAX_PROPERTY_BYTES + 1),
            true,
        )));
        let calls = Arc::new(FakeCalls::default());
        let player = fake_player(&address, state.clone(), calls.clone());
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(address), ping).unwrap();

        wait_until(|| calls.metadata_queries.load(Ordering::Acquire) == 1);
        state.lock().unwrap().title = "recovered title".into();
        emit_invalidated_properties(&player);
        let snapshot = wait_for(&media, |snapshot| snapshot.title == "recovered title");
        assert!(snapshot.available);
        assert_eq!(calls.metadata_queries.load(Ordering::Acquire), 2);
        thread::sleep(QUERY_RETRY_DELAY + Duration::from_millis(100));
        assert_eq!(calls.metadata_queries.load(Ordering::Acquire), 2);

        drop(media);
        drop(player);
        bus.stop();
    }

    #[test]
    fn retired_lifetime_discards_its_pending_discovery_retry() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let state = Arc::new(Mutex::new(FakeState::new(
            &"x".repeat(MAX_PROPERTY_BYTES + 1),
            true,
        )));
        let calls = Arc::new(FakeCalls::default());
        let player = fake_player(&address, state.clone(), calls.clone());
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(address), ping).unwrap();

        wait_until(|| calls.metadata_queries.load(Ordering::Acquire) == 1);
        state.lock().unwrap().title = "replacement title".into();
        assert!(async_io::block_on(player.release_name(PLAYER_NAME)).unwrap());
        async_io::block_on(player.request_name(PLAYER_NAME)).unwrap();
        let snapshot = wait_for(&media, |snapshot| snapshot.title == "replacement title");
        assert!(snapshot.available);
        assert_eq!(calls.metadata_queries.load(Ordering::Acquire), 2);
        thread::sleep(QUERY_RETRY_DELAY + Duration::from_millis(100));
        assert_eq!(calls.metadata_queries.load(Ordering::Acquire), 2);

        drop(media);
        drop(player);
        bus.stop();
    }

    #[test]
    fn persistent_discovery_failures_are_rate_limited_and_shutdown_is_bounded() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let calls = Arc::new(FakeCalls::default());
        let player = fake_player(
            &address,
            Arc::new(Mutex::new(FakeState::new(
                &"x".repeat(MAX_PROPERTY_BYTES + 1),
                true,
            ))),
            calls.clone(),
        );
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(address), ping).unwrap();

        wait_until(|| calls.metadata_queries.load(Ordering::Acquire) == 1);
        thread::sleep(Duration::from_millis(400));
        assert_eq!(calls.metadata_queries.load(Ordering::Acquire), 1);
        wait_until(|| calls.metadata_queries.load(Ordering::Acquire) == 2);
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        drop(media);
        assert!(started.elapsed() < Duration::from_millis(500));

        drop(player);
        bus.stop();
    }

    #[test]
    fn shutdown_is_bounded_while_a_player_query_is_stalled() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let mut state = FakeState::new("stalled title", false);
        state.stall_metadata = true;
        let calls = Arc::new(FakeCalls::default());
        let player = fake_player(&address, Arc::new(Mutex::new(state)), calls.clone());
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(address), ping).unwrap();
        wait_until(|| calls.metadata_queries.load(Ordering::Acquire) > 0);

        let started = Instant::now();
        drop(media);
        assert!(started.elapsed() < METHOD_TIMEOUT + Duration::from_secs(2));

        drop(player);
        bus.stop();
    }

    #[test]
    fn shutdown_is_bounded_while_bus_authentication_is_stalled() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let listener = UnixListener::bind(&socket).unwrap();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let stream = listener.accept().unwrap().0;
            accepted_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(stream);
        });
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(format!("unix:path={}", socket.display())), ping).unwrap();
        accepted_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let (dropped_tx, dropped_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(media);
            dropped_tx.send(()).unwrap();
        });
        let bounded = dropped_rx.recv_timeout(Duration::from_secs(3)).is_ok();
        release_tx.send(()).unwrap();
        server.join().unwrap();
        dropper.join().unwrap();
        assert!(bounded);
    }

    #[test]
    fn shutdown_is_bounded_while_established_bus_stops_reading() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let player = fake_player(
            &address,
            Arc::new(Mutex::new(FakeState::new("playing", true))),
            Arc::new(FakeCalls::default()),
        );
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let media = Media::start_at(Some(address), ping).unwrap();
        let selected = wait_for(&media, |snapshot| snapshot.available).selected_id;

        let bus_pid = bus.child.as_ref().unwrap().id() as libc::pid_t;
        // SAFETY: this test owns the child process and checks the result.
        assert_eq!(unsafe { libc::kill(bus_pid, libc::SIGSTOP) }, 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut accepted = 0_usize;
        let mut last_progress = Instant::now();
        while Instant::now() < deadline {
            match media.send(MediaAction::PlayPause, Some(&selected)) {
                Ok(()) => {
                    accepted += 1;
                    last_progress = Instant::now();
                }
                Err("media command queue is full") => {
                    if accepted > MAX_COMMANDS
                        && last_progress.elapsed() >= Duration::from_millis(500)
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("unexpected send failure: {error}"),
            }
        }
        assert!(accepted > MAX_COMMANDS, "worker made no queue progress");

        let (dropped_tx, dropped_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(media);
            dropped_tx.send(()).unwrap();
        });
        let bounded = dropped_rx.recv_timeout(Duration::from_secs(3)).is_ok();
        bus.stop();
        drop(player);
        dropper.join().unwrap();
        assert!(bounded, "drop blocked after {accepted} accepted actions");
    }

    fn target(id: &str, name: &str, owner: &str) -> Target {
        let mut model = Model::default();
        model.initialize([Player {
            id: id.into(),
            name: name.into(),
            owner: owner.into(),
            playing: true,
            title: String::new(),
            artist: String::new(),
            art_url: String::new(),
            identity: String::new(),
            desktop_entry: String::new(),
            can_play: true,
            can_pause: true,
            can_next: true,
            can_previous: true,
            can_control: true,
        }]);
        model.snapshot().selected.unwrap()
    }

    struct Bus {
        child: Option<Child>,
    }

    impl Bus {
        fn start(address: &str) -> Self {
            let mut child = ProcessCommand::new("dbus-daemon")
                .args([
                    "--session",
                    "--nofork",
                    "--nopidfile",
                    "--print-address=1",
                    &format!("--address={address}"),
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("dbus-daemon must be installed for media tests");
            let mut output = BufReader::new(child.stdout.take().unwrap());
            let mut ready = String::new();
            output.read_line(&mut ready).unwrap();
            assert!(ready.trim().starts_with(address));
            Self { child: Some(child) }
        }

        fn stop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            self.stop();
        }
    }

    struct FakeState {
        title: String,
        playing: bool,
        can_play: bool,
        can_pause: bool,
        stall_metadata: bool,
        metadata_delay: Duration,
    }

    impl FakeState {
        fn new(title: &str, playing: bool) -> Self {
            Self {
                title: title.into(),
                playing,
                can_play: !playing,
                can_pause: playing,
                stall_metadata: false,
                metadata_delay: Duration::ZERO,
            }
        }
    }

    #[derive(Default)]
    struct FakeCalls {
        play: AtomicUsize,
        pause: AtomicUsize,
        play_pause: AtomicUsize,
        metadata_queries: AtomicUsize,
    }

    struct FakeRoot;

    #[zbus::interface(name = "org.mpris.MediaPlayer2")]
    impl FakeRoot {
        #[zbus(property)]
        fn identity(&self) -> &str {
            "thing test player"
        }

        #[zbus(property)]
        fn desktop_entry(&self) -> &str {
            "thing-test"
        }
    }

    struct FakePlayer {
        state: Arc<Mutex<FakeState>>,
        calls: Arc<FakeCalls>,
    }

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl FakePlayer {
        fn play_pause(&self) {
            self.calls.play_pause.fetch_add(1, Ordering::Release);
        }

        fn play(&self) {
            self.calls.play.fetch_add(1, Ordering::Release);
        }

        fn pause(&self) {
            self.calls.pause.fetch_add(1, Ordering::Release);
        }

        fn next(&self) {}

        fn previous(&self) {}

        fn stop(&self) {}

        #[zbus(property)]
        fn playback_status(&self) -> &str {
            if self.state.lock().unwrap().playing {
                "Playing"
            } else {
                "Paused"
            }
        }

        #[zbus(property)]
        async fn metadata(&self) -> HashMap<String, OwnedValue> {
            let (stall, delay, title) = {
                let state = self.state.lock().unwrap();
                (
                    state.stall_metadata,
                    state.metadata_delay,
                    state.title.clone(),
                )
            };
            self.calls.metadata_queries.fetch_add(1, Ordering::Release);
            if stall {
                std::future::pending::<()>().await;
            }
            if !delay.is_zero() {
                Timer::after(delay).await;
            }
            HashMap::from([
                (
                    "xesam:title".into(),
                    Value::from(title.as_str()).try_into().unwrap(),
                ),
                (
                    "xesam:artist".into(),
                    Value::from(vec!["test artist"]).try_into().unwrap(),
                ),
                (
                    "mpris:artUrl".into(),
                    Value::from("file:///tmp/test-cover.png")
                        .try_into()
                        .unwrap(),
                ),
            ])
        }

        #[zbus(property)]
        fn can_play(&self) -> bool {
            self.state.lock().unwrap().can_play
        }

        #[zbus(property)]
        fn can_pause(&self) -> bool {
            self.state.lock().unwrap().can_pause
        }

        #[zbus(property)]
        fn can_go_next(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_go_previous(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_control(&self) -> bool {
            true
        }
    }

    fn fake_player(
        address: &str,
        state: Arc<Mutex<FakeState>>,
        calls: Arc<FakeCalls>,
    ) -> Connection {
        fake_player_named(address, PLAYER_NAME, state, calls)
    }

    fn fake_player_named(
        address: &str,
        name: &str,
        state: Arc<Mutex<FakeState>>,
        calls: Arc<FakeCalls>,
    ) -> Connection {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let result = async_io::block_on(async {
                zbus::connection::Builder::address(address)?
                    .name(name)?
                    .serve_at(MPRIS_PATH, FakeRoot)?
                    .serve_at(
                        MPRIS_PATH,
                        FakePlayer {
                            state: state.clone(),
                            calls: calls.clone(),
                        },
                    )?
                    .build()
                    .await
            });
            match result {
                Ok(connection) => return connection,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("cannot start fake media player: {error}"),
            }
        }
    }

    fn emit_invalidated_properties(connection: &Connection) {
        async_io::block_on(connection.emit_signal(
            None::<&str>,
            MPRIS_PATH,
            PROPERTIES_INTERFACE,
            "PropertiesChanged",
            &(
                PLAYER_INTERFACE,
                HashMap::<String, OwnedValue>::new(),
                vec!["PlaybackStatus", "Metadata"],
            ),
        ))
        .unwrap();
    }

    fn emit_malformed_properties(connection: &Connection) {
        async_io::block_on(connection.emit_signal(
            None::<&str>,
            MPRIS_PATH,
            PROPERTIES_INTERFACE,
            "PropertiesChanged",
            &(PLAYER_INTERFACE, 7u32),
        ))
        .unwrap();
    }

    fn wait_for(media: &Media, predicate: impl Fn(&MediaSnapshot) -> bool) -> MediaSnapshot {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut observed = Vec::new();
        while Instant::now() < deadline {
            if let Some(snapshot) = media.take_snapshot() {
                observed.push(snapshot.clone());
                if predicate(&snapshot) {
                    return snapshot;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for media state; observed {observed:?}");
    }

    fn wait_until(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for fake media action");
    }
}
