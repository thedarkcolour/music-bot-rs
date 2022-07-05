mod commands;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::env;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use songbird::tracks::TrackHandle;
use tokio::sync::RwLock;

use serde::Deserialize;
use serenity::model::id::GuildId;
use serenity::prelude::*;
use serenity::{async_trait};

use serenity::framework::standard::{StandardFramework};
use serenity::model::gateway::Ready;
use songbird::input::Input;
use songbird::{SerenityInit}; // type alias to not conflict with serenity
use serenity::Client as SerenityClient;
use reqwest::Client;

struct Handler;
#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
    }
}

#[tokio::main]
async fn main() {
    // get environment vars to run the bot
    let discord_token = env::var("DISCORD_TOKEN").expect("Missing Discord bot token");
    let prefix = env::var("COMMAND_PREFIX").expect("Missing bot prefix");
    let youtube_key = env::var("YOUTUBE_KEY").expect("Missing YouTube API key");
    let spotify_id = env::var("SPOTIFY_CLIENT_ID").expect("Missing Spotify Client ID");
    let spotify_secret = env::var("SPOTIFY_CLIENT_SECRET").expect("Missing Spotify Client secret");

    let framework = StandardFramework::new()
        .configure(|c| 
            c
                .prefix(prefix)
                .case_insensitivity(true)
        )
        .after(commands::after)
        .help(&commands::MY_HELP)
        .group(&commands::GENERAL_GROUP); // refers to general struct

    let api_access = ApiAccess::new(youtube_key, spotify_id, spotify_secret).await;

    // let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;
    let mut client = SerenityClient::builder(discord_token)
        .event_handler(Handler)
        .register_songbird()
        .type_map_insert::<ApiAccessKey>(Arc::new(api_access))
        .type_map_insert::<PerServerQueueAccessKey>(Arc::new(PerServerQueue { map: RwLock::new(HashMap::new()) }))
        .framework(framework)
        .await
        .expect("Error creating serenity client");

    if let Err(why) = client.start().await {
        println!("An error occurred while running the client: {:?}", why);
    }
}

// Key to get api access from context type map
struct ApiAccessKey;
impl TypeMapKey for ApiAccessKey {
    type Value = Arc<ApiAccess>;
}

struct ApiAccess {
    youtube_key: String,
    http: Arc<Client>,
    spotify_token: Arc<RwLock<String>>,
}

impl ApiAccess {
    async fn new(youtube_key: String, spotify_id: String, spotify_secret: String) -> ApiAccess {
        let http = Arc::new(Client::new());

        let token_lock = Arc::new(RwLock::new(String::new()));

        {
            let mut token = (&token_lock).write().await;
            *token = generate_spotify_token(&http, &spotify_id, &spotify_secret).await;
        }

        ApiAccess {
            youtube_key,
            http,
            spotify_token: token_lock,
        }
    }

    async fn search_yt(&self, query: impl std::fmt::Display) -> SearchResult {
        // we do not need &part=snippet
        // todo look into using a form instead of format! for the args
        let req = format!("https://www.googleapis.com/youtube/v3/search?part=snippet&maxResults=5&type=video&q={}&key={}", query, self.youtube_key);
        let res = self.http.get(req)
            .send()
            .await
            .expect("Failed to access YouTube API");
        res.json::<SearchResult>()
            .await
            .expect(&format!("Error parsing search response"))
    }

    async fn get_video_info(&self, video_id: &str) -> YouTubeVideo {
        #[derive(Deserialize)]
        struct ListResponse {
            items: [VideoObject; 1],
        }
        #[derive(Deserialize)]
        struct VideoObject {
            snippet: SnippetPart,
        }
        let res = self.http.get(format!("https://www.googleapis.com/youtube/v3/videos?part=snippet&id={}&key={}", video_id, self.youtube_key))
            .send()
            .await
            .expect("Failed to access YouTube API");
        let list = res.json::<ListResponse>()
            .await
            .expect("Error parsing response");
        let video = &list.items[0];

        YouTubeVideo {
            name: video.snippet.title.clone(),
            channel: video.snippet.channel_title.clone(),
            duration: self.get_video_duration(video_id).await,
            id: video_id.to_string(),
        }
    }

    async fn get_video_duration(&self, video_id: impl AsRef<str> + std::fmt::Display) -> Duration {
        #[derive(Deserialize)]
        struct VideoListResponse {
            items: [VideoDetailsResponse; 1],
        }
        #[derive(Deserialize)]
        struct VideoDetailsResponse {
            #[serde(rename="contentDetails")]
            content_details: ContentDetails,
        }
        #[derive(Deserialize)]
        struct ContentDetails {
            // ISO 8601 duration string
            duration: String,
        }
        let url = format!("https://www.googleapis.com/youtube/v3/videos?part=contentDetails&id={}&key={}", video_id, self.youtube_key);
        let video_list = self.http.get(url)
            .send()
            .await
            .expect("Failed to access YouTube API")
            .json::<VideoListResponse>()
            .await
            .expect("Error parsing response");

        let duration = &(video_list.items[0]).content_details.duration;

        duration_from_iso_8601(duration)
    }

    async fn get_spotify_track(&self, track_id: &str) -> SpotifyTrack {
        let res = self.http.get(format!("https://api.spotify.com/v1/tracks/{}", track_id))
            .bearer_auth(self.spotify_token.read().await)
            .header("Content-Type", "application/json")
            .send()
            .await
            .expect("Failed to access Spotify API");
        res.json::<SpotifyTrack>()
            .await
            .expect("Error parsing response")
    }
}

#[derive(Deserialize)]
struct SearchResult {
    items: [VideoObject; 5],
}

#[derive(Deserialize)]
struct VideoObject {
    id: VideoId,
    snippet: SnippetPart,
}

#[derive(Deserialize)]
struct VideoId {
    #[serde(rename="videoId")]
    video_id: String,
}

#[derive(Deserialize)]
struct SnippetPart {
    title: String,
    #[serde(rename="channelTitle")]
    channel_title: String,
}

struct YouTubeVideo {
    name: String,
    channel: String,
    duration: Duration,
    id: String,
}

impl YouTubeVideo {
    fn url(&self) -> String {
        format!("https://youtube.com/watch?v={}", self.id)
    }

    fn as_song(&self, author: String) -> Song {
        Song {
            // decode HTML characters
            title: self.name.clone().replace("&#39;", "'"),
            artist: self.channel.clone(),
            author,
            duration: self.duration,
            source: SongSource::YouTube { 
                id: self.id.clone(),
                url: self.url(),
            },
            handle: None,
        }
    }
}

struct VideoDetails {
    duration: Duration,
}

fn duration_from_iso_8601(duration_string: &str) -> Duration {
    Duration::from(iso8601::Duration::from_str(duration_string).expect("Failed to parse ISO 8601 duration string"))
}

#[derive(Deserialize)]
struct SpotifyTrack {
    artists: Vec<SpotifyArtist>,
    duration_ms: u64,
    name: String,
}

#[derive(Deserialize)]
struct SpotifyArtist {
    name: String,
}

// This method uses the client credentials flow.
async fn generate_spotify_token(client: &Client, client_id: &String, client_secret: &String) -> String {
    let params = [("grant_type", "client_credentials")];

    let res = client.post("https://accounts.spotify.com/api/token")
        .header("Authorization", format!("Basic {}", base64::encode(format!("{}:{}", client_id, client_secret))))
        .form(&params)
        .send()
        .await
        .expect("Failed to generate new Spotify token");

    let credentials = res.json::<ClientCredentialsResponse>()
        .await
        .expect("Error parsing response");

    credentials.access_token
}

#[derive(Deserialize)]
struct ClientCredentialsResponse {
    access_token: String,
}

struct PerServerQueueAccessKey;
impl TypeMapKey for PerServerQueueAccessKey {
    type Value = Arc<PerServerQueue>;
}

struct PerServerQueue {
    map: RwLock<HashMap<GuildId, Arc<Mutex<ServerQueue>>>>,
}

impl PerServerQueue {
    // Gets a server queue or uses the write lock to create a new one
    async fn queue_or_create(&self, guild_id: &GuildId) -> Arc<Mutex<ServerQueue>> {
        let map = self.map.read().await;

        if !map.contains_key(guild_id) {
            // drop the read lock
            drop(map);
            let mut map = self.map.write().await;
            map.insert(*guild_id, Arc::new(Mutex::new(ServerQueue { now_playing: None, queue: VecDeque::new() })));
            map.get(guild_id).unwrap().clone()
        } else {
            map.get(guild_id).unwrap().clone()
        }
    }

    // Obtains a read lock.
    //fn queue(&self, guild_id: &GuildId) -> Option<&ServerQueue> {
    //    self.map.get(guild_id)
    //}
}

// These are only accessed from a Mutex so no thread handling should be necessary
struct ServerQueue {
    now_playing: Option<Song>,
    queue: VecDeque<Song>,
}

impl ServerQueue {
    fn skip(&mut self) {
        self.now_playing = None;
    }

    fn stop(&mut self) {
        self.now_playing = None;
        self.queue.clear();
    }

    // Shifts the songs forward after the front song ends
    fn shift_queue(&mut self) {
        self.now_playing = self.queue.pop_front();
    }
}

struct Song {
    title: String,
    artist: String,
    author: String,
    duration: Duration,
    source: SongSource,
    handle: Option<TrackHandle>,
}

impl Song {
    fn title_with_link(&self) -> String {
        match &self.source {
            SongSource::YouTube { id: _, url } => format!("[{}]({})", self.title, url),
            _ => format!("{} (Local files)", self.title),
        }
    }
}

enum SongSource {
    YouTube { id: String, url: String }
}

impl SongSource {
    async fn as_input(&self) -> songbird::input::error::Result<Input> {
        match self {
            SongSource::YouTube { id: _, url } => songbird::input::ytdl(url).await,
        }
    }
}