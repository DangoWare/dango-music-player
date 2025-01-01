//! The [Controller] is the input and output for the entire
//! player. It manages queues, playback, library access, and
//! other functions

use chrono::TimeDelta;
use crossbeam::atomic::AtomicCell;
use crossbeam_channel::{Receiver, Sender};
use kushi::Queue;
use kushi::{QueueError, QueueItem};
use parking_lot::RwLock;
use prismriver::{Error as PrismError, Prismriver};
use serde::{Deserialize, Serialize};
use serde_json::to_string_pretty;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

use crate::config::ConfigError;
use crate::music_storage::library::Song;
use crate::music_storage::playlist::{ExternalPlaylist, Playlist};
use crate::{config::Config, music_storage::library::MusicLibrary};

use super::connections::{ConnectionsInput, ConnectionsNotification, ControllerConnections};
use super::controller_handle::{LibraryCommandInput, PlayerCommandInput, QueueCommandInput};
use super::queue::{QueueAlbum, QueueSong};

pub struct Controller();

type QueueItem_ = QueueItem<QueueSong, QueueAlbum>;

#[derive(Error, Debug)]
pub enum ControllerError {
    #[error("{0:?}")]
    QueueError(#[from] QueueError),
    #[error("{0:?}")]
    PlayerError(#[from] prismriver::Error),
    #[error("{0:?}")]
    ConfigError(#[from] ConfigError),
}

// TODO: move this to a different location to be used elsewhere
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PlayerLocation {
    Test,
    Library,
    Playlist(Uuid),
    File,
    Custom,
}

#[derive(Debug, Clone)]
pub struct MailMan<Tx: Send, Rx: Send> {
    tx: async_channel::Sender<Tx>,
    rx: async_channel::Receiver<Rx>,
}

impl<Tx: Send, Rx: Send> MailMan<Tx, Rx> {
    pub fn double() -> (MailMan<Tx, Rx>, MailMan<Rx, Tx>) {
        let (tx, rx) = async_channel::unbounded::<Tx>();
        let (tx1, rx1) = async_channel::unbounded::<Rx>();

        (MailMan { tx, rx: rx1 }, MailMan { tx: tx1, rx })
    }

    pub async fn send(&self, mail: Tx) -> Result<(), async_channel::SendError<Tx>> {
        self.tx.send(mail).await
    }

    pub async fn recv(&self) -> Result<Rx, async_channel::RecvError> {
        self.rx.recv().await
    }
}

#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub enum PlayerCommand {
    NextSong,
    PrevSong,
    Pause,
    Play,
    Stop,
    Seek(i64),
    Enqueue(usize),
    SetVolume(f32),
    PlayNow(Uuid, PlayerLocation),
}

#[derive(Debug, PartialEq, Clone)]
pub enum PlayerResponse {
    Empty(Result<(), PlayerError>),
    NowPlaying(Result<Song, QueueError>),
}

#[derive(Error, Debug, PartialEq, Clone)]
pub enum PlayerError {
    #[error("{0}")]
    QueueError(#[from] QueueError),
    #[error("{0}")]
    Prismriver(#[from] PrismError),
}

#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub enum LibraryCommand {
    Song(Uuid),
    AllSongs,
    GetLibrary,
    ExternalPlaylist(Uuid),
    Playlist(Uuid),
    ImportM3UPlayList(PathBuf),
    Save,
    Playlists,
}

#[derive(Debug, Clone)]
pub enum LibraryResponse {
    Ok,
    Song(Song, usize),
    AllSongs(Vec<Song>),
    Library(MusicLibrary),
    ExternalPlaylist(ExternalPlaylist),
    Playlist(Playlist),
    ImportM3UPlayList(Uuid, String),
    Playlists(Vec<(Uuid, String)>),
}

#[derive(Debug, PartialEq, Clone)]
pub enum QueueCommand {
    Append(QueueItem_, bool),
    Next,
    Prev,
    GetIndex(usize),
    NowPlaying,
    Get,
    Clear,
    Remove(usize),
}

#[derive(Debug, PartialEq, Clone)]
pub enum QueueResponse {
    Empty(Result<(), QueueError>),
    Item(Result<QueueItem_, QueueError>),
    GetAll(Vec<QueueItem_>),
}

pub struct ControllerInput {
    player_mail: (
        async_channel::Sender<PlayerCommandInput>,
        async_channel::Receiver<PlayerCommandInput>,
    ),
    lib_mail: (
        async_channel::Sender<LibraryCommandInput>,
        async_channel::Receiver<LibraryCommandInput>,
    ),
    queue_mail: (
        async_channel::Sender<QueueCommandInput>,
        async_channel::Receiver<QueueCommandInput>,
    ),
    library: MusicLibrary,
    config: Arc<RwLock<Config>>,
    playback_info: Arc<AtomicCell<PlaybackInfo>>,
    notify_next_song: Sender<Song>,
    connections: Option<ConnectionsInput>,
}

pub struct ControllerHandle {
    pub(super) lib_mail_rx: async_channel::Sender<LibraryCommandInput>,
    pub(super) player_mail_rx: async_channel::Sender<PlayerCommandInput>,
    pub(super) queue_mail_rx: async_channel::Sender<QueueCommandInput>,
}

impl ControllerHandle {
    pub fn new(
        library: MusicLibrary,
        config: Arc<RwLock<Config>>,
        connections: Option<ConnectionsInput>,
    ) -> (
        Self,
        ControllerInput,
        Arc<AtomicCell<PlaybackInfo>>,
        Receiver<Song>,
    ) {
        let (lib_mail_rx, lib_mail_tx) = async_channel::unbounded();
        let (player_mail_rx, player_mail_tx) = async_channel::unbounded();
        let (queue_mail_rx, queue_mail_tx) = async_channel::unbounded();
        let playback_info = Arc::new(AtomicCell::new(PlaybackInfo::default()));
        let notify_next_song = crossbeam::channel::unbounded::<Song>();
        (
            ControllerHandle {
                lib_mail_rx: lib_mail_rx.clone(),
                player_mail_rx: player_mail_rx.clone(),
                queue_mail_rx: queue_mail_rx.clone(),
            },
            ControllerInput {
                player_mail: (player_mail_rx, player_mail_tx),
                lib_mail: (lib_mail_rx, lib_mail_tx),
                queue_mail: (queue_mail_rx, queue_mail_tx),
                library,
                config,
                playback_info: Arc::clone(&playback_info),
                notify_next_song: notify_next_song.0,
                connections,
            },
            playback_info,
            notify_next_song.1,
        )
    }
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct ControllerState {
    pub(super) path: PathBuf,
    pub(super) volume: f32,
    pub(super) now_playing: Uuid,
}

impl ControllerState {
    pub(super) fn new(path: PathBuf) -> Self {
        ControllerState {
            path,
            volume: 0.35,
            ..Default::default()
        }
    }

    pub(super) fn write_file(&self) -> Result<(), std::io::Error> {
        OpenOptions::new()
            .truncate(true)
            .create(true)
            .write(true)
            .open(&self.path)
            .unwrap()
            .write_all(&to_string_pretty(self)?.into_bytes())?;
        Ok(())
    }

    pub(super) fn read_file(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let state = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        Ok(state)
    }
}

#[allow(unused_variables)]
impl Controller {
    pub async fn start(
        ControllerInput {
            player_mail,
            lib_mail,
            queue_mail,
            mut library,
            config,
            playback_info,
            notify_next_song,
            connections,
        }: ControllerInput,
    ) -> Result<(), Box<dyn Error>> {
        let queue: Queue<QueueSong, QueueAlbum> = Queue {
            items: Vec::new(),
            played: Vec::new(),
            loop_: false,
            shuffle: None,
        };

        let state = {
            let path = &config.read().state_path;
            if let Ok(state) = ControllerState::read_file(path) {
                state
            } else {
                ControllerState::new(path.clone())
            }
        };

        std::thread::scope(|scope| {
            let player = Prismriver::new();
            let player_state = player.state.clone();
            let player_timing = player.get_timing_recv();
            let finished_tx = player.get_finished_recv();
            let (notifications_rx, notifications_tx) =
                crossbeam_channel::unbounded::<ConnectionsNotification>();

            let a = scope.spawn({
                let queue_mail = queue_mail.clone();
                let _notifications_rx = notifications_rx.clone();
                let _config = config.clone();
                move || {
                    futures::executor::block_on(async {
                        moro::async_scope!(|scope| {
                            println!("async scope created");

                            let _lib_mail = lib_mail.0.clone();
                            let _queue_mail = queue_mail.0.clone();
                            scope.spawn(async move {
                                Controller::player_command_loop(
                                    player,
                                    player_mail.1,
                                    _queue_mail,
                                    _lib_mail,
                                    _notifications_rx,
                                    state,
                                )
                                .await
                                .unwrap();
                            });
                            scope.spawn(async {
                                Controller::library_loop(lib_mail.1, &mut library, _config)
                                    .await
                                    .unwrap();
                            });
                        })
                        .await;
                    })
                }
            });

            let b = scope.spawn(|| {
                futures::executor::block_on(async {
                    Controller::queue_loop(queue, queue_mail.1).await;
                })
            });

            let c = scope.spawn(|| {
                Controller::player_monitor_loop(
                    player_state,
                    player_timing,
                    finished_tx,
                    player_mail.0,
                    notify_next_song,
                    notifications_rx,
                    playback_info,
                )
                .unwrap();
            });

            if let Some(inner) = connections {
                dbg!(&inner);
                let d = scope.spawn(|| {
                    Controller::handle_connections(
                        config,
                        ControllerConnections {
                            notifications_tx,
                            inner,
                        },
                    );
                });
            }
            a.join().unwrap();
            b.join().unwrap();
            c.join().unwrap();
        });

        Ok(())
    }
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct PlaybackInfo {
    pub position: Option<TimeDelta>,
    pub duration: Option<TimeDelta>,
}
