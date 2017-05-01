use std::io::{self, BufRead, BufReader};
use std::net::TcpStream;
use std::path::PathBuf;
use std::thread;

use ssh2;
use serde_json;

use futures::stream::BoxStream;
use futures::sync::mpsc::channel;
use futures::{Future, Sink, Stream};

#[derive(Deserialize, Debug)]
pub struct User {
    name: String,
    username: String,
    email: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    #[serde(rename="type")]
    approval_type: String,
    description: String,
    value: String,
    old_value: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PatchSet {
    number: String,
    revision: String,
    parents: Vec<String>,
    #[serde(rename="ref")]
    reference: String,
    uploader: User,
    created_on: u32,
    author: User,
    is_draft: bool,
    kind: String,
    size_insertions: u32,
    size_deletions: u32,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    project: String,
    branch: String,
    id: String,
    number: String,
    subject: String,
    owner: User,
    url: String,
    commit_message: String,
    status: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ChangeKey {
    id: String,
}

// Only specific event are accepted by this type by design!
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    author: User,
    approvals: Vec<Approval>,
    comment: Option<String>,
    #[serde(rename="patchSet")]
    patchset: PatchSet,
    change: Change,
    project: String,
    #[serde(rename="refName")]
    ref_name: String,
    #[serde(rename="changeKey")]
    changekey: ChangeKey,
    #[serde(rename="type")]
    event_type: String,
    #[serde(rename="eventCreatedOn")]
    created_on: u32,
}

#[derive(Debug)]
pub enum StreamError {
    Io(io::Error),
    Parse(serde_json::Error),
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

pub fn event_stream(host: String,
                    port: u16,
                    username: String,
                    priv_key_path: PathBuf)
                    -> BoxStream<Event, StreamError> {
    let hostport = format!("{}:{}", host, port);
    let mut session = ssh2::Session::new().unwrap();

    let (mut tx, rx) = channel(1);
    thread::spawn(move || {
        // Connect to the local SSH server
        let tcp = TcpStream::connect(hostport).unwrap();
        session.handshake(&tcp).unwrap();

        // Try to authenticate
        session.userauth_pubkey_file(&username, None, &priv_key_path, None)
            .unwrap();

        let mut ssh_channel = session.channel_session().unwrap();
        ssh_channel.exec("gerrit stream-events").unwrap();

        let buf_channel = BufReader::new(ssh_channel);
        for line in buf_channel.lines() {
            match tx.send(line).wait() {
                Ok(s) => tx = s,
                Err(_) => break,
            }
        }
    });

    // TODO: Right now, we are interested only in +1/-1/+2/-2 events. When we have implemented all
    // event types mappings, we can provide here full parsing by removing the filtering.
    rx.then(|event| {
            // event from our channel cannot fail
            let json: String = event.unwrap()?;
            Ok(serde_json::from_str(&json).ok())
        })
        .filter(|event| event.is_some())
        .map(|event| event.unwrap()) // we cannot fail here, since we filtered all None's
        .boxed()
}