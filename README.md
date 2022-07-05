## music-bot-rs ♡
Discord music bot written in Rust. Supports Spotify URLs, YouTube URLs, and YouTube search.
Requires the Python package `youtube-dl`, which can be installed using the following command:
```sh
pip install youtube-dl
```
Also requires FFmpeg to be installed and included somewhere in your system path.
To run, an example PowerShell script that sets the required environment variables is provided:
```ps1
# Discord Bot token from Discord Developer portal
$Env:DISCORD_TOKEN="...";
# Bot prefix
$Env:COMMAND_PREFIX="!";
# YouTube Data API Key created in Google Developer console
$Env:YOUTUBE_KEY="...";
# Client credentials from Spotify developer console
$Env:SPOTIFY_CLIENT_ID="...";
$Env:SPOTIFY_CLIENT_SECRET="...";
cargo watch -x run;
```