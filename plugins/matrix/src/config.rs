//! The plugin's section of `node.yaml`:
//!
//! ```yaml
//! plugins:
//!   - name: matrix
//!     command: fleetlayer-matrix
//!     grants: [event.publish]
//!     config:
//!       homeserver: https://matrix.example.com
//!       user: "@fleetlayer:example.com"
//!       password_file: ~/.config/fleetlayer/matrix-password
//!       rooms:
//!         - room: "#infrastructure-alerts:example.com"
//!           routes:
//!             - match: "(?i)backup|restic"
//!               subject: backup
//! ```

use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

/// The most rooms one plugin reads.
const MAX_ROOMS: usize = 100;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The homeserver's URL (`https://matrix.example.com`).
    pub homeserver: String,
    /// The bot's own Matrix user (`@fleetlayer:example.com`).
    pub user: String,
    /// A file only this Unix user can read, holding the bot's password and nothing else. Read once, to sign in; the
    /// device and its token are kept in the state directory after that.
    pub password_file: String,
    /// The rooms to read.
    pub rooms: Vec<RoomConfig>,
    /// Dead man's switch rooms: a message in one (Alertmanager's Watchdog, say) is a heartbeat, passed on as an HTTP
    /// GET to a ping URL, so an alert path that falls silent alerts by its silence. Nothing about it reaches a channel.
    #[serde(default)]
    pub heartbeats: Vec<HeartbeatRoom>,
    /// The device's name, which other members see. Default "Fleetlayer (<node>)".
    #[serde(default)]
    pub device_name: Option<String>,
    /// The name of the `tools:` entry in `node.yaml` that shares `fleetlayer-matrix mcp` (say `matrix`), so each
    /// event tells the agent how to reply (`fleetlayer tools call matrix@<node> post …`). Default: no reply line.
    #[serde(default)]
    pub tool: Option<String>,
    /// Where the device, its keys, the sync position and the outbox live. Default
    /// `~/.local/state/fleetlayer/matrix`.
    #[serde(default)]
    pub state_dir: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomConfig {
    /// The room, by ID (`!abc:example.com`) or alias (`#alerts:example.com`).
    pub room: String,
    /// The Fleetlayer channel its messages are published on. Default: the alias's name (`alerts`); a room given by ID
    /// needs one.
    #[serde(default)]
    pub channel: Option<String>,
    /// Whether agents may post to it through the `post` tool (still under the access policy). Default true.
    #[serde(default = "yes")]
    pub post: bool,
    /// Rules that give matching messages a subject, so a session subscribed to that subject gets them: the first
    /// that matches wins. A message no rule matches has no subject.
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// A regular expression, matched against the message's text (`(?i)` for any case).
    #[serde(rename = "match")]
    pub pattern: String,
    /// Only messages from this sender (`@alertmanager:example.com`). Default any sender.
    #[serde(default)]
    pub sender: Option<String>,
    /// The subject matching messages get.
    pub subject: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatRoom {
    /// The room, by ID or alias.
    pub room: String,
    /// A file only this Unix user can read, holding the ping URL and nothing else.
    pub ping_url_file: String,
}

fn yes() -> bool {
    true
}

/// A room as the reader uses it: its channel, and its routes compiled.
#[derive(Debug, Clone)]
pub struct Room {
    pub spec: String,
    pub channel: String,
    pub post: bool,
    pub routes: Vec<Route>,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub pattern: Regex,
    pub sender: Option<String>,
    pub subject: String,
}

impl Room {
    /// The subject of a message from `sender` saying `text`: the first matching route's, if any.
    pub fn subject(&self, sender: &str, text: &str) -> Option<&str> {
        self.routes
            .iter()
            .find(|r| r.sender.as_deref().is_none_or(|s| s == sender) && r.pattern.is_match(text))
            .map(|r| r.subject.as_str())
    }
}

/// A channel name Fleetlayer accepts, as the node checks it.
pub fn channel_ok(c: &str) -> bool {
    !c.is_empty() && c.len() <= 100 && c.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))
}

fn room_spec_ok(r: &str) -> bool {
    (r.starts_with('!') || r.starts_with('#')) && r.contains(':') && !r.chars().any(char::is_whitespace)
}

impl Config {
    /// Every problem at once, or the rooms ready to read.
    pub fn check(&self) -> Result<Vec<Room>, String> {
        let mut problems = vec![];
        if !self.homeserver.starts_with("https://") && !self.homeserver.starts_with("http://") {
            problems.push("homeserver must be an http(s) URL".to_string());
        }
        if !self.user.starts_with('@') || !self.user.contains(':') {
            problems.push("user must be a full Matrix ID such as @fleetlayer:example.com".into());
        }
        if self.rooms.is_empty() {
            problems.push("rooms names no room".into());
        }
        if self.rooms.len() > MAX_ROOMS {
            problems.push(format!("at most {MAX_ROOMS} rooms"));
        }
        let mut rooms: Vec<Room> = vec![];
        for r in &self.rooms {
            if !room_spec_ok(&r.room) {
                problems.push(format!("{:?} isn't a room ID (!abc:example.com) or alias (#name:example.com)", r.room));
                continue;
            }
            let channel = match (&r.channel, r.room.strip_prefix('#')) {
                (Some(c), _) => c.clone(),
                (None, Some(alias)) => alias.split(':').next().unwrap_or_default().to_string(),
                (None, None) => {
                    problems.push(format!("{}: a room given by ID needs a channel", r.room));
                    continue;
                }
            };
            if !channel_ok(&channel) {
                problems.push(format!(
                    "{}: channel {channel:?} must be 1 to 100 letters, digits and - _ . : (set channel:)",
                    r.room
                ));
                continue;
            }
            if rooms.iter().any(|x| x.spec == r.room) {
                problems.push(format!("{} is listed twice", r.room));
            }
            if rooms.iter().any(|x| x.channel == channel) {
                problems.push(format!("two rooms publish on channel {channel}: give one its own channel:"));
            }
            let mut routes = vec![];
            for (i, rt) in r.routes.iter().enumerate() {
                match Regex::new(&rt.pattern) {
                    Ok(pattern) if !rt.subject.trim().is_empty() && rt.subject.len() <= 200 => routes.push(Route {
                        pattern,
                        sender: rt.sender.clone(),
                        subject: rt.subject.trim().to_string(),
                    }),
                    Ok(_) => problems.push(format!("{} route {}: subject must be 1 to 200 bytes", r.room, i + 1)),
                    Err(e) => problems.push(format!("{} route {}: {e}", r.room, i + 1)),
                }
            }
            rooms.push(Room { spec: r.room.clone(), channel, post: r.post, routes });
        }
        for h in &self.heartbeats {
            if !room_spec_ok(&h.room) {
                problems.push(format!("heartbeats: {:?} isn't a room ID or alias", h.room));
            } else if self.rooms.iter().any(|r| r.room == h.room) {
                problems.push(format!("heartbeats: {} is also in rooms; a heartbeat room is only pinged", h.room));
            }
        }
        if problems.is_empty() { Ok(rooms) } else { Err(problems.join("; ")) }
    }

    pub fn state_dir(&self, home: &Path) -> PathBuf {
        self.state_dir.as_deref().map(|d| expand(home, d)).unwrap_or_else(|| default_state_dir(home))
    }
}

pub fn default_state_dir(home: &Path) -> PathBuf {
    home.join(".local/state/fleetlayer/matrix")
}

pub fn expand(home: &Path, p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if p == "~" => home.to_path_buf(),
        None => PathBuf::from(p),
    }
}

/// A secret file's contents, trimmed. Refused when others can read it.
pub fn read_secret_file(path: &Path) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(format!("{} can be read by other users: chmod 600 it", path.display()));
        }
    }
    let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let s = s.trim().to_string();
    if s.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok(s)
}
