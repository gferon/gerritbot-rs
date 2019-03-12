use std::net::SocketAddr;
use std::rc::Rc;
use std::{error, fmt, io};

use futures::future::Future;
use futures::sync::mpsc::channel;
use futures::{Sink, Stream};
use hyper;
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use notify_rust::Notification;
use regex::Regex;
use reqwest;
use rusoto_core;
use serde;
use serde::{Deserialize, Serialize};
use serde_json;
use serde_json::json;

mod sqs;

//
// Helper functions
//

/// Try to get json from the given url with basic token authorization.
fn get_json_with_token(url: &str, token: &str) -> reqwest::Result<reqwest::Response> {
    reqwest::Client::new()
        .get(url)
        .bearer_auth(token)
        .header(http::header::ACCEPT, "application/json")
        .send()
}

/// Try to post json to the given url with basic token authorization.
fn post_with_token<T>(url: &str, token: &str, data: &T) -> reqwest::Result<reqwest::Response>
where
    T: Serialize,
{
    reqwest::Client::new()
        .post(url)
        .bearer_auth(token)
        .header(http::header::ACCEPT, "application/json")
        .json(&data)
        .send()
}

/// Try to post json to the given url with basic token authorization.
fn delete_with_token(url: &str, token: &str) -> reqwest::Result<reqwest::Response> {
    reqwest::Client::new()
        .delete(url)
        .bearer_auth(token)
        .header(http::header::ACCEPT, "application/json")
        .send()
}

//
// Spark data model
//

/// Spark id of the user
pub type PersonId = String;

/// Email of the user
pub type Email = String;

/// Webhook's post request from Spark API
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Post {
    actor_id: String,
    app_id: String,
    created: String,
    created_by: String,
    pub data: Message,
    event: String,
    id: String,
    name: String,
    org_id: String,
    owned_by: String,
    resource: String,
    status: String,
    target_url: String,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    created: Option<String>,
    id: String,
    pub person_email: String,
    pub person_id: String,
    room_id: String,
    room_type: String,

    // a message contained in a post does not have text loaded
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PersonDetails {
    id: String,
    emails: Vec<String>,
    display_name: String,
    nick_name: Option<String>,
    org_id: String,
    created: String,
    last_activity: Option<String>,
    status: Option<String>,
    #[serde(rename = "type")]
    person_type: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Webhook {
    id: String,
    name: String,
    target_url: String,
    resource: String,
    event: String,
    org_id: String,
    created_by: String,
    app_id: String,
    owned_by: String,
    status: String,
    created: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Webhooks {
    items: Vec<Webhook>,
}

//
// Client
//

pub trait SparkClient {
    fn id(&self) -> &str;
    fn reply(&self, person_id: &str, msg: &str);
    fn get_message(&self, message_id: &str) -> Result<Message, Error>;
}

#[derive(Debug, Clone)]
pub struct WebClient {
    url: String,
    bot_token: String,
    pub bot_id: String,
}

#[derive(Debug)]
pub enum Error {
    ReqwestError(reqwest::Error),
    SqsError(sqs::Error),
    JsonError(serde_json::Error),
    RegisterWebhook(String),
    DeleteWebhook(String),
    IoError(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::ReqwestError(ref err) => fmt::Display::fmt(err, f),
            Error::SqsError(ref err) => fmt::Display::fmt(err, f),
            Error::JsonError(ref err) => fmt::Display::fmt(err, f),
            Error::RegisterWebhook(ref msg) | Error::DeleteWebhook(ref msg) => {
                fmt::Display::fmt(msg, f)
            }
            Error::IoError(ref err) => fmt::Display::fmt(err, f),
        }
    }
}

impl error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::ReqwestError(ref err) => err.description(),
            Error::SqsError(ref err) => err.description(),
            Error::JsonError(ref err) => err.description(),
            Error::RegisterWebhook(ref msg) | Error::DeleteWebhook(ref msg) => msg,
            Error::IoError(ref err) => err.description(),
        }
    }

    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match *self {
            Error::ReqwestError(ref err) => err.source(),
            Error::SqsError(ref err) => err.source(),
            Error::JsonError(ref err) => err.source(),
            Error::RegisterWebhook(_) | Error::DeleteWebhook(_) => None,
            Error::IoError(ref err) => err.source(),
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Error::ReqwestError(err)
    }
}

impl From<sqs::Error> for Error {
    fn from(err: sqs::Error) -> Self {
        Error::SqsError(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::JsonError(err)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::IoError(err)
    }
}

impl WebClient {
    pub fn new(
        spark_api_url: String,
        bot_token: String,
        webhook_url: Option<String>,
    ) -> Result<Self, Error> {
        let mut client = Self {
            url: spark_api_url,
            bot_token: bot_token,
            bot_id: String::new(),
        };

        client.bot_id = client.get_bot_id()?;
        debug!("Bot id: {}", client.bot_id);

        if let Some(webhook_url) = webhook_url {
            client.replace_webhook_url(&webhook_url)?;
            info!("Registered Spark's webhook url: {}", webhook_url);
        }

        Ok(client)
    }

    fn get_bot_id(&self) -> Result<String, Error> {
        let resp = get_json_with_token(&(self.url.clone() + "/people/me"), &self.bot_token)?;
        let details: PersonDetails = serde_json::from_reader(resp)?;
        Ok(details.id)
    }

    fn register_webhook(&self, url: &str) -> Result<(), Error> {
        let json = json!({
            "name": "gerritbot",
            "targetUrl": String::from(url),
            "resource": "messages",
            "event": "created"
        });
        post_with_token(&(self.url.clone() + "/webhooks"), &self.bot_token, &json)
            .map_err(Error::from)
            .and_then(|resp| {
                if resp.status() != http::StatusCode::OK {
                    Err(Error::RegisterWebhook(format!(
                        "Could not register Spark's webhook: {}",
                        resp.status()
                    )))
                } else {
                    Ok(())
                }
            })
    }

    fn list_webhooks(&self) -> Result<Webhooks, Error> {
        let resp = get_json_with_token(&(self.url.clone() + "/webhooks"), &self.bot_token)?;
        let webhooks: Webhooks = serde_json::from_reader(resp)?;
        Ok(webhooks)
    }

    fn delete_webhook(&self, id: &str) -> Result<(), Error> {
        delete_with_token(&(self.url.clone() + "/webhooks/" + id), &self.bot_token)
            .map_err(Error::from)
            .and_then(|resp| {
                if resp.status() != http::StatusCode::NO_CONTENT
                    && resp.status() != http::StatusCode::NOT_FOUND
                {
                    Err(Error::DeleteWebhook(format!(
                        "Could not delete webhook: {}",
                        resp.status()
                    )))
                } else {
                    Ok(())
                }
            })
    }

    fn replace_webhook_url(&self, url: &str) -> Result<(), Error> {
        // remove all other webhooks
        let webhooks = self.list_webhooks()?;
        let to_remove = webhooks.items.into_iter().filter_map(|webhook| {
            if webhook.resource == "messages" && webhook.event == "created" {
                Some(webhook)
            } else {
                None
            }
        });
        for webhook in to_remove {
            self.delete_webhook(&webhook.id)?;
            debug!("Removed webhook from Spark: {}", webhook.target_url);
        }

        // register new webhook
        self.register_webhook(url)
    }
}

impl SparkClient for WebClient {
    fn id(&self) -> &str {
        &self.bot_id
    }

    fn reply(&self, person_id: &str, msg: &str) {
        let json = json!({
            "toPersonId": person_id,
            "markdown": msg,
        });
        let res = post_with_token(&(self.url.clone() + "/messages"), &self.bot_token, &json);
        if let Err(err) = res {
            error!("Could not reply to gerrit: {:?}", err);
        }
    }

    fn get_message(&self, message_id: &str) -> Result<Message, Error> {
        let resp = get_json_with_token(
            &(self.url.clone() + "/messages/" + message_id),
            &self.bot_token,
        )?;
        serde_json::from_reader(resp).map_err(Error::from)
    }
}

#[derive(Debug, Clone)]
pub struct ConsoleClient {
    stdin_enabled: bool,
}

impl ConsoleClient {
    /// Create a console client which resolves the message text always with a placeholder text.
    pub fn new() -> Self {
        Self {
            stdin_enabled: false,
        }
    }

    // Create a console client which resolves the message text from stdin.
    pub fn _with_stdin() -> Self {
        Self {
            stdin_enabled: true,
        }
    }
}

impl SparkClient for ConsoleClient {
    fn id(&self) -> &str {
        "console-client"
    }

    fn reply(&self, person_id: &str, msg: &str) {
        print!("Would reply to {}: {}", person_id, msg);
    }

    fn get_message(&self, message_id: &str) -> Result<Message, Error> {
        if self.stdin_enabled {
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
            serde_json::from_str(&line).map_err(Error::from)
        } else {
            let mut message = Message::default();
            message.id = message_id.into();
            message.text = "Placeholder text".into();
            Ok(message)
        }
    }
}

pub struct NotificationClient;

impl NotificationClient {
    pub fn new() -> Self {
        Self {}
    }
}

impl SparkClient for NotificationClient {
    fn id(&self) -> &str {
        "notify-dbus-client"
    }

    fn reply(&self, _person_id: &str, msg: &str) {
        Notification::new()
            .summary("Gerrit Bot")
            .body(msg)
            .show()
            .unwrap();
    }

    fn get_message(&self, message_id: &str) -> Result<Message, Error> {
        let mut message = Message::default();
        message.id = message_id.into();
        message.text = "Placeholder text".into();
        Ok(message)
    }
}

#[derive(Debug, Clone)]
pub struct CommandMessage {
    pub sender_email: String,
    pub sender_id: String,
    pub command: Command,
}

#[derive(Debug, Clone)]
pub enum Command {
    Enable,
    Disable,
    ShowStatus,
    ShowHelp,
    ShowFilter,
    EnableFilter,
    DisableFilter,
    SetFilter(String),
    Unknown,
}

impl Message {
    /// Load text from Spark for a received message
    /// Note: Spark does not send the text with the message to the registered post hook.
    pub fn load_text<C: SparkClient + ?Sized>(&mut self, client: &C) -> Result<(), Error> {
        let msg = client.get_message(&self.id)?;
        self.text = msg.text;
        Ok(())
    }

    /// Convert Spark message to command
    pub fn into_command(self) -> CommandMessage {
        lazy_static! {
            static ref FILTER_REGEX: Regex = Regex::new(r"(?i)^filter (.*)$").unwrap();
        };

        let sender_email = self.person_email;
        let sender_id = self.person_id;
        let command = match &self.text.trim().to_lowercase()[..] {
            "enable" => Command::Enable,
            "disable" => Command::Disable,
            "status" => Command::ShowStatus,
            "help" => Command::ShowHelp,
            "filter" => Command::ShowFilter,
            "filter enable" => Command::EnableFilter,
            "filter disable" => Command::DisableFilter,
            _ => FILTER_REGEX
                .captures(&self.text.trim()[..])
                .and_then(|cap| cap.get(1))
                .map(|m| Command::SetFilter(m.as_str().to_string()))
                .unwrap_or(Command::Unknown),
        };

        CommandMessage {
            sender_email,
            sender_id,
            command,
        }
    }
}

fn reject_webhook_request(
    request: &hyper::Request<hyper::Body>,
) -> Option<hyper::Response<hyper::Body>> {
    use hyper::{Body, Response};

    if request.uri() != "/" {
        // only accept requests at "/"
        Some(
            Response::builder()
                .status(http::StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap(),
        )
    } else if request.method() != http::Method::POST {
        // only accept POST
        Some(
            Response::builder()
                .status(http::StatusCode::METHOD_NOT_ALLOWED)
                .body(Body::empty())
                .unwrap(),
        )
    } else if !request
        .headers()
        .get(http::header::CONTENT_TYPE)
        .map(|v| v.as_bytes().starts_with(&b"application/json"[..]))
        .unwrap_or(false)
    {
        // require "content-type: application/json"
        Some(
            Response::builder()
                .status(http::StatusCode::UNSUPPORTED_MEDIA_TYPE)
                .body(Body::empty())
                .unwrap(),
        )
    } else {
        None
    }
}

pub fn webhook_event_stream(listen_address: &SocketAddr) -> impl Stream<Item = Post, Error = ()> {
    use hyper::{Body, Response};
    let (tx, rx) = channel(1);
    let listen_address = listen_address.clone();

    // very simple webhook listener
    let server = hyper::Server::bind(&listen_address).serve(move || {
        info!("listening to Spark on {}", listen_address);
        let tx = tx.clone();

        hyper::service::service_fn_ok(move |request: hyper::Request<Body>| {
            debug!("webhook request: {:?}", request);

            if let Some(error_response) = reject_webhook_request(&request) {
                // reject requests we don't understand
                warn!("rejecting webhook request: {:?}", error_response);
                error_response
            } else {
                let tx = tx.clone();
                // now try to read the body
                let f = request
                    // consume the request and use the body
                    .into_body()
                    .map_err(|e| error!("failed to read post body: {}", e))
                    // collect body chunks into vector
                    // is there a better way to do this, maybe?
                    .fold(Vec::new(), |mut v, chunk| {
                        v.extend_from_slice(chunk.as_ref());
                        futures::future::ok(v)
                    })
                    // decode the json
                    .and_then(|v| {
                        serde_json::from_slice::<Post>(&v)
                            .map_err(|e| error!("failed to decode post body: {}", e))
                    })
                    // send through channel
                    .and_then(|post| {
                        tx.send(post.clone())
                            .map_err(|e| error!("failed to send post body: {}", e))
                            .map(|_| ())
                    });

                // spawn a future so all of the above actually happens
                tokio::spawn(f);

                Response::new(Body::empty())
            }
        })
    });

    tokio::spawn(server.map_err(|e| error!("webhook server error: {}", e)));

    rx
}

pub fn sqs_event_stream<C: SparkClient + 'static + ?Sized>(
    client: Rc<C>,
    sqs_url: String,
    sqs_region: rusoto_core::Region,
) -> Result<Box<dyn Stream<Item = CommandMessage, Error = String>>, Error> {
    let bot_id = String::from(client.id());
    let sqs_stream = sqs::sqs_receiver(sqs_url, sqs_region)?;
    let sqs_stream = sqs_stream
        .filter_map(|sqs_message| {
            if let Some(body) = sqs_message.body {
                let new_post: Post = match serde_json::from_str(&body) {
                    Ok(post) => post,
                    Err(err) => {
                        error!("Could not parse post: {}", err);
                        return None;
                    }
                };
                Some(new_post.data)
            } else {
                None
            }
        })
        .filter(move |msg| msg.person_id != bot_id)
        .filter_map(move |mut msg| {
            debug!("Loading text for message: {:#?}", msg);
            if let Err(err) = msg.load_text(&*client) {
                error!("Could not load post's text: {}", err);
                return None;
            }
            Some(msg)
        })
        .map(|msg| msg.into_command())
        .map_err(|err| format!("Error from Spark: {:?}", err));
    Ok(Box::new(sqs_stream))
}
