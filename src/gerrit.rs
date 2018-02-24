use std::io::{self, BufRead, BufReader};
use std::net::TcpStream;
use std::path::PathBuf;
use std::thread;

use ssh2;
use ssh2::Channel;
use serde_json;

use futures::sync::mpsc::{channel, Receiver, Sender};
use futures::{Future, Sink, Stream};

use bot;

/// Gerrit username
pub type Username = String;

#[derive(Deserialize, Debug, Clone)]
pub struct User {
    pub name: Option<String>,
    pub username: Username,
    pub email: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    #[serde(rename = "type")]
    pub approval_type: String,
    pub description: String,
    pub value: String,
    pub old_value: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PatchSet {
    pub number: String,
    pub revision: String,
    pub parents: Vec<String>,
    #[serde(rename = "ref")]
    pub reference: String,
    pub uploader: User,
    pub created_on: u32,
    pub author: User,
    pub is_draft: bool,
    pub kind: String,
    pub size_insertions: i32,
    pub size_deletions: i32,
    pub comments: Option<Vec<InlineComment>>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InlineComment {
    pub file: String,
    pub line: u32,
    pub reviewer: User,
    pub message: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub project: String,
    pub branch: String,
    pub id: String,
    pub number: String,
    pub subject: String,
    pub topic: Option<String>,
    pub owner: User,
    pub url: String,
    pub commit_message: String,
    pub status: String,
    pub current_patch_set: Option<PatchSet>,
}

#[derive(Deserialize, Debug, Eq, PartialEq, Hash, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChangeKey {
    pub id: String,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub enum EventType {
    #[serde(rename = "reviewer-added")]
    ReviewerAdded,
    #[serde(rename = "comment-added")]
    CommentAdded,
}

// Only specific events are accepted by this type by design!
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub author: Option<User>,
    pub uploader: Option<User>,
    pub approvals: Option<Vec<Approval>>,
    pub reviewer: Option<User>,
    pub comment: Option<String>,
    #[serde(rename = "patchSet")]
    pub patchset: PatchSet,
    pub change: Change,
    pub project: String,
    #[serde(rename = "refName")]
    pub ref_name: String,
    #[serde(rename = "changeKey")]
    pub changekey: ChangeKey,
    #[serde(rename = "type")]
    pub event_type: EventType,
    #[serde(rename = "eventCreatedOn")]
    pub created_on: u32,
}

impl Event {
    pub fn into_action(self) -> bot::Action {
        if self.event_type == EventType::CommentAdded && self.approvals.is_some() {
            bot::Action::UpdateApprovals(Box::new(self))
        } else if self.event_type == EventType::ReviewerAdded {
            bot::Action::ReviewerAdded(Box::new(self))
        } else {
            bot::Action::NoOp
        }
    }
}

#[derive(Debug)]
pub enum StreamError {
    Io(io::Error),
    Parse(serde_json::Error),
    Terminated(String /* reason */),
}

impl From<io::Error> for StreamError {
    fn from(err: io::Error) -> StreamError {
        StreamError::Io(err)
    }
}

impl From<serde_json::Error> for StreamError {
    fn from(err: serde_json::Error) -> StreamError {
        StreamError::Parse(err)
    }
}

fn get_pub_key_path(priv_key_path: &PathBuf) -> PathBuf {
    let mut pub_key_path = PathBuf::from(priv_key_path.to_str().unwrap());
    pub_key_path.set_extension("pub");
    pub_key_path
}

fn send_terminate_msg<T>(
    tx: &Sender<Result<String, StreamError>>,
    reason: String,
) -> Result<T, ()> {
    let _ = tx.clone().send(Err(StreamError::Terminated(reason))).wait();
    Err(())
}

pub struct GerritConnection {
    pub session: ssh2::Session,
    /// tcp has to be kept alive with session together, even if it is never used directly
    _tcp: TcpStream,
}

impl GerritConnection {
    pub fn new(session: ssh2::Session, tcp: TcpStream) -> GerritConnection {
        GerritConnection {
            session: session,
            _tcp: tcp,
        }
    }
}

pub fn connect_to_gerrit(
    host: &str,
    username: &str,
    priv_key_path: &PathBuf,
) -> Result<GerritConnection, String> {
    let pub_key_path = get_pub_key_path(&priv_key_path);
    debug!("Will use public key: {}", pub_key_path.to_str().unwrap());

    let mut session = ssh2::Session::new().unwrap();

    let tcp = TcpStream::connect(host).or_else(|err| {
        Err(format!(
            "Could not connect to gerrit at {}: {:?}",
            host, err
        ))
    })?;

    session
        .handshake(&tcp)
        .or_else(|err| Err(format!("Could not connect to gerrit: {:?}", err)))?;

    // Try to authenticate
    session
        .userauth_pubkey_file(username, Some(&pub_key_path), priv_key_path, None)
        .or_else(|err| Err(format!("Could not authenticate: {:?}", err)))?;

    Ok(GerritConnection::new(session, tcp))
}

fn receiver_into_event_stream(
    rx: Receiver<Result<String, StreamError>>,
) -> Box<Stream<Item = bot::Action, Error = String>> {
    let stream = rx.then(|event| {
        // parse each json message as event (if we did not get an error)
        event.unwrap().map(|event| {
            let json: String = event;
            let res = serde_json::from_str(&json);
            debug!("Incoming Gerrit event: {:?}", res);
            res.ok()
        })
    }).filter_map(|event| event.map(Event::into_action))
        .map_err(|err| format!("Stream error from Gerrit: {:?}", err));
    Box::new(stream)
}

pub fn event_stream(
    host: String,
    username: String,
    priv_key_path: PathBuf,
) -> Box<Stream<Item = bot::Action, Error = String>> {
    let (main_tx, rx) = channel(1);
    thread::spawn(move || -> Result<(), ()> {
        loop {
            info!("(Re)connecting to Gerrit over SSH: {}", host);
            let conn = connect_to_gerrit(&host, &username, &priv_key_path).or_else(|err| {
                send_terminate_msg(
                    &main_tx.clone(),
                    format!("Could not connect to Gerrit: {}", err),
                )
            })?;

            let mut ssh_channel = conn.session.channel_session().or_else(|err| {
                send_terminate_msg(
                    &main_tx.clone(),
                    format!("Could not open SSH channel: {:?}", err),
                )
            })?;
            ssh_channel
                .exec("gerrit stream-events -s comment-added -s reviewer-added")
                .or_else(|err| {
                    send_terminate_msg(
                        &main_tx.clone(),
                        format!(
                            "Could not execute gerrit stream-event command over ssh: {:?}",
                            err
                        ),
                    )
                })?;
            info!("Connected to Gerrit.");

            let buf_channel = BufReader::new(ssh_channel);
            let mut tx = main_tx.clone();
            for line in buf_channel.lines() {
                if let Ok(line) = line {
                    match tx.clone().send(Ok(line)).wait() {
                        Ok(s) => tx = s,
                        Err(err) => {
                            error!("Cannot send message through channel {:?}", err);
                            break;
                        }
                    }
                } else {
                    error!("Could not read line from buffer. Will drop connection.");
                    break;
                }
            }
        }
    });

    receiver_into_event_stream(rx)
}

#[cfg(test)]
mod test {
    use super::{get_pub_key_path, PathBuf};

    #[test]
    fn test_get_pub_key_path() {
        let result = get_pub_key_path(&PathBuf::from("some_priv_key"));
        assert!(result == PathBuf::from("some_priv_key.pub"));
    }
}

pub fn query(mut ssh_channel: Channel, change_id: &str) -> Result<Change, serde_json::Error> {
    let query = format!(
        "gerrit query --format JSON --current-patch-set --comments {}",
        change_id
    );
    ssh_channel.exec(&query).unwrap();

    let buf_channel = BufReader::new(ssh_channel);
    let line = buf_channel.lines().next();

    // event from our channel cannot fail
    let json: String = line.unwrap().ok().unwrap();
    let res: Result<Change, _> = serde_json::from_str(&json);
    debug!("[D] {:?} for json: {}", res, json);

    res
}
