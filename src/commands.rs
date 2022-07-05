use std::collections::HashSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use serenity::framework::standard::{CommandResult, Args, HelpOptions, CommandGroup, help_commands};
use serenity::framework::standard::macros::{command, group, hook, help};
use serenity::http::{Http, CacheHttp};
use serenity::model::guild::Guild;
use serenity::{prelude::*, async_trait};
use serenity::Result;
use serenity::model::channel::Message;
use serenity::model::id::{ChannelId, UserId};
use songbird::{TrackEvent, Event, EventHandler as VoiceEventHandler, EventContext, Call};
use tokio::sync::MutexGuard;

use crate::{ApiAccessKey, ApiAccess, PerServerQueue, PerServerQueueAccessKey, Song, YouTubeVideo, SongSource, ServerQueue};

#[group("general")]
#[commands(summon, play, now_playing, queue, skip)]
pub(crate) struct General;

fn user_vc(guild: &Guild, user: &UserId) -> Option<ChannelId> {
    guild
        .voice_states
        .get(user)
        .and_then(|voice_state| voice_state.channel_id)
}

#[command]
#[only_in(guilds)]
#[aliases("join")]
async fn summon(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;

    // if the author is in a vc
    if let Some(author_vc) = user_vc(&guild, &msg.author.id) {
        // get songbird manager from the type map
        let manager = songbird::get(ctx)
            .await
            .expect("Songbird voice client placed in at initialization.")
            .clone();

        // attempt to join voice channel
        let (handle_lock, success) = manager.join(guild_id, author_vc).await;

        if success.is_ok() {
            msg.channel_id
                .say(&ctx.http, &format!("Joined {}", author_vc.mention()))
                .await?;
        }
    } else {
        must_be_in_vc(ctx, msg).await?;
    }

    Ok(())
}

async fn must_be_in_vc(ctx: &Context, msg: &Message) -> CommandResult {
    msg.channel_id.say(ctx, "Must be in a voice channel to use this command").await?;
    Ok(())
}

async fn nothing_playing(ctx: &Context, msg: &Message) -> CommandResult {
    msg.channel_id.say(ctx, "Nothing playing").await?;
    Ok(())
}

#[command]
#[aliases("np", "nowplaying")]
async fn now_playing(ctx: &Context, msg: &Message, args: Args) -> CommandResult {
    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice client passed in at initialization.")
        .clone();
    
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;

    if manager.get(guild_id).is_some() {
        let queues = get_queues(ctx).await.clone();
        let server_queue_lock = queues.queue_or_create(&guild_id).await.clone();
        let server_queue = server_queue_lock.lock().await;
        let avatar_url = ctx.http.get_current_user().await?.avatar_url();

        if let Some(song) = &server_queue.now_playing {
            let progress_bar = "▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬";
            let current_position = "0:00";
            let track_duration = format_duration(&song.duration);
            msg.channel_id.send_message(ctx.http.clone(), |m| {
                m.embed(|embed| {
                    embed.author(|author| {
                        author.name("Now Playing 🎵");

                        if let Some(url) = avatar_url {
                            author.icon_url(url);
                        }
                        author
                    })
                        .description(format!("{}\n\n`{}`\n\n`{} \\ {}`\n\n`Requested by:` {}", song.title_with_link(), progress_bar, current_position, track_duration, song.author));

                    if let SongSource::YouTube { id, url: _ } = &song.source {
                        embed.thumbnail(format!("https://img.youtube.com/vi/{}/mqdefault.jpg", id));
                    }

                    embed
                })
            }).await?; 
        } else {
            nothing_playing(ctx, msg).await?;
        }
    } else {
        nothing_playing(ctx, msg).await?;
    }

    Ok(())
}

fn format_duration(duration: &Duration) -> String {
    let secs = duration.as_secs();
    let mins = secs / 60;
    let hours = mins / 60;

    if hours != 0 {
        format!("{:02}:{:02}:{:02}", hours, mins % 60, secs % 60)
    } else {
        format!("{:02}:{:02}", mins % 60, secs % 60)
    }
}

#[command]
#[aliases("p")]
async fn play(ctx: &Context, msg: &Message, args: Args) -> CommandResult {
    let message = args.message();
    
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;
    
    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice client passed in at initialization.")
        .clone();

    // Retrieve call ref or obtain one by joining the call
    let mut call_lock = manager.get(guild_id);
    
    if call_lock.is_none() {
        if let Some(author_vc) = user_vc(&guild, &msg.author.id) {
            let (handle_lock, _) = manager.join(guild_id, author_vc).await;
            call_lock = Some(handle_lock);
        } else {
            must_be_in_vc(ctx, msg).await?;
        }
    }
        
    // Should only be None if the sender is not in vc
    if let Some(call_lock) = call_lock {
        let call = call_lock.lock().await;

        // Check if user is in same channel as bot
        if call.current_channel().unwrap().0 != user_vc(&guild, &msg.author.id).map_or(0, |val| val.0) {
            check_msg(msg.channel_id.say(&ctx.http, "You must be in the same voice channel to use this command.").await)
        }

        // Searches the song
        if let Some(mut song) = get_song(ctx, msg, message).await {
            // get server's track queue
            // clones are necessary to avoid thread deadlock (arcs must stay within their own threads)
            let queues = get_queues(ctx).await.clone();
            let server_queue_lock = queues.queue_or_create(&guild_id).await.clone();
            let mut server_queue = server_queue_lock.lock().await;

            if server_queue.now_playing.is_some() {
                let avatar_url = ctx.http.get_current_user().await?.avatar_url();
                let linked_title = &song.title_with_link().clone();
                let artist = &song.artist.clone();
                let track_duration = format_duration(&song.duration);
    
                check_msg(msg.channel_id.send_message(&ctx.http, |m| {
                    m.embed(|e| {
                        e.author(|a| {
                            a.name("Added to queue");
                            
                            if let Some(avatar_url) = avatar_url {
                                a.icon_url(avatar_url);
                            }
                            a
                        })
                            .description(format!("**{}**", linked_title))
                            .field("Channel", format!("{}", artist), true)
                            .field("Song Duration", format!("{}", track_duration), true)
                            .field("Time until playing", "todo", true)
                            .field("Position in queue", server_queue.queue.len() + 1, false);
                        if let SongSource::YouTube { id, url: _ } = &song.source {
                            e.thumbnail(format!("https://img.youtube.com/vi/{}/mqdefault.jpg", id));
                        }
                        e
                    })
                }).await);
    
                server_queue.queue.push_back(song);
            } else {
                if play_song(ctx.http.clone(), msg.channel_id, call_lock.clone(), Some(call), &mut song, server_queue_lock.clone()).await {
                    return Ok(());
                }

                // move at the very end
                server_queue.now_playing = Some(song);
            }
        } else {
            check_msg(msg.channel_id.say(&ctx.http, "No matches").await);
        }
    } else {
        check_msg(msg.channel_id.say(&ctx.http, "Cannot play this type of link").await);
    }

    Ok(())
}

// Obtains a lock from call_lock, make sure locks are not held earlier in the call stack
async fn play_song(http: Arc<Http>, text_channel: ChannelId, call_lock: Arc<Mutex<Call>>, call: Option<MutexGuard<'_, Call>>, song: &mut Song, server_queue: Arc<Mutex<ServerQueue>>) -> bool {
    let source = match song.source.as_input().await {
        Ok(source) => source,
        Err(why) => {
            println!("Err starting source: {:?}", why);
            check_msg(text_channel.say(http, "Error sourcing ffmpeg").await);

            return true;
        },
    };

    // cannot use .unwrap_or because locking val must be lazy
    // cannot use .unwrap_or_else because closure cannot be async
    let mut call = match call  {
        Some(call) => call,
        None => call_lock.lock().await,
    };
    let track = call.play_source(source);
    let send_http = http.clone();
    let send_call_lock = Arc::downgrade(&call_lock.clone());

    // song ends
    let _ = track.add_event(
        Event::Track(TrackEvent::End),
        SongEndNotifier {
            text_channel: text_channel.clone(),
            http: send_http,
            server_queue: server_queue.clone(),
            call_lock: send_call_lock,
        },
    );

    // move track into song
    song.handle.replace(track);

    check_msg(text_channel.say(http, format!("**Playing** 🎶 `{}` - Now!", song.title)).await);

    false
}

struct SongEndNotifier {
    // sending message
    text_channel: ChannelId,
    http: Arc<Http>,
    // shifting queue
    server_queue: Arc<Mutex<ServerQueue>>,
    // playing song
    call_lock: Weak<Mutex<Call>>,
}

#[async_trait]
impl VoiceEventHandler for SongEndNotifier {
    async fn act(&self, _: &EventContext<'_>) -> Option<Event> {
        if let Some(call_lock) = self.call_lock.upgrade() {
            let mut queue = self.server_queue.lock().await;
        
            queue.shift_queue();
            if let Some(now_playing) = &mut queue.now_playing {
                play_song(self.http.clone(), self.text_channel, call_lock, None, now_playing, self.server_queue.clone()).await;
            }
        }

        //check_msg(self.text_channel.say(&self.http, "Song ended!").await);
        None
    }
}

#[command]
#[aliases("q")]
async fn queue(ctx: &Context, msg: &Message, _: Args) -> CommandResult {
    let manager = songbird::get(ctx)
        .await
        .expect("Songbird not yet initialized")
        .clone();
 
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;

    if !manager.get(guild_id).is_none() {
        let queues = get_queues(ctx).await.clone();
        let server_queue_lock = queues.queue_or_create(&guild_id).await.clone();
        let server_queue = server_queue_lock.lock().await;

        let mut description = "__Now Playing:__\n".to_owned();
        if let Some(now_playing) = &server_queue.now_playing {
            description.push_str(&format!("{} | `{} Requested by: {}`", now_playing.title_with_link(), format_duration(&now_playing.duration), now_playing.author))
        } else {
            description.push_str("Nothing");
        }
        if !server_queue.queue.is_empty() {
            description.push_str("\n\n__Up Next:__\n");
            for (i, song) in server_queue.queue.iter().enumerate().filter(|(i , _)| *i < 10) {
                description.push_str(&format!("`{}.` {} | `{} Requested by: {}`", i + 1, song.title_with_link(), format_duration(&song.duration), song.author));
                
                if i + 1 < server_queue.queue.len() {
                    description.push_str("\n\n");
                }
            }
        }

        msg.channel_id.send_message(ctx.http.clone(), |m| {
            m.embed(|e| {
                e.title(format!("Queue for {}", guild.name))
                    .description(description)
            })
        }).await?;
    }

    Ok(())
}

#[command]
#[only_in(guilds)]
#[aliases("s", "fs")]
async fn skip(ctx: &Context, msg: &Message, _: Args) -> CommandResult {
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;

    let manager = songbird::get(ctx)
        .await
        .expect("Songbird not yet initialized")
        .clone();

    if let Some(handler_lock) = manager.get(guild_id) {
        let mut handler = handler_lock.lock().await;
        let _ = handler.stop();
        let queue_lock = get_queues(ctx)
            .await
            .queue_or_create(&guild_id)
            .await;
        let mut queue = queue_lock.lock().await;
        if let Some(now_playing) = &queue.now_playing {
            now_playing.handle.as_ref().unwrap().send(songbird::tracks::TrackCommand::Stop)?;
    
            check_msg(msg.channel_id.say(&ctx.http, "Skipped!").await);
        } else {
            nothing_playing(ctx, msg).await?;
        }
    } else {
        check_msg(msg.channel_id.say(&ctx.http, "Not in a voice channel").await);
    }
    Ok(())
}

#[hook]
pub(crate) async fn after(_: &Context, _: &Message, command_name: &str, command_result: CommandResult) {
    match command_result {
        Err(why) => println!(
            "Command '{}' returned error {:?} => {}",
            command_name, why, why
        ),
        _ => (),
    }
}

#[help]
#[command_not_found_text = "Could not find: `{}`."]
#[max_levenshtein_distance(3)]
#[individual_command_tip=""]
#[strikethrough_commands_tip_in_guild=""]
async fn my_help(
    ctx: &Context,
    msg: &Message,
    args: Args,
    help_options: &'static HelpOptions,
    groups: &[&'static CommandGroup],
    owners: HashSet<UserId>,
) -> CommandResult {
    let _ =  help_commands::with_embeds(ctx, msg, args, help_options, groups, owners).await;
    Ok(())
}

fn check_msg(result: Result<Message>) {
    if let Err(why) = result {
        println!("Error sending message: {:?}", why);
    }
}

async fn get_song(ctx: &Context, msg: &Message, message: &str) -> Option<Song> {
    if message.starts_with("http") {
        if message.contains("spotify.com/track/") {
            // Spotify link
            let api_access = get_api_access(ctx).await.clone();
            let track_id = &message.split("track/").nth(1).unwrap()[ .. 22];
            let track = api_access.get_spotify_track(track_id).await;
            let video = first_yt_result(ctx, &format!("{} {} lyrics explicit", track.name, track.artists.get(0).map_or("", |artist| &artist.name))).await;

            Some(video.as_song(msg.author.tag()))
        } else if message.contains("soundcloud") {
            // Soundcloud link
            None
        } else {
            // YouTube Link
            let link = message.to_owned();
            let id = link.split("?v=").nth(1);

            if let Some(id) = id {
                if id.len() < 11 {
                    return None;
                }

                let id = &id[ .. 11 ];
                let api_access = get_api_access(ctx).await.clone();
                let track = api_access.get_video_info(id).await;

                return Some(track.as_song(msg.author.tag()));
            }

            None
        }
    } else {
        let result = first_yt_result(ctx, message).await;
        Some(result.as_song(msg.author.tag()))
    }
}

async fn first_yt_result(ctx: &Context, query: &str) -> YouTubeVideo {
    let api_access = get_api_access(ctx).await.clone();
    let results = api_access.search_yt(query).await;
    let first = &results.items[0];
    let id = &first.id.video_id;
    let duration = api_access.get_video_duration(id).await;

    YouTubeVideo {
        name: first.snippet.title.clone(),
        channel: first.snippet.channel_title.clone(),
        duration,
        id: id.clone(),
    }
}

async fn get_api_access(ctx: &Context) -> Arc<ApiAccess> {
    ctx.data.read().await.get::<ApiAccessKey>().cloned().expect("API Access not yet initialized")
}

async fn get_queues(ctx: &Context) -> Arc<PerServerQueue> {
    ctx.data.read().await.get::<PerServerQueueAccessKey>().cloned().expect("PerServerQueue not yet initialized")
}
