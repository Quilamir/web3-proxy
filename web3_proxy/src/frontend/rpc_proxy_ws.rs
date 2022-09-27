use super::authorization::{
    bearer_is_authorized, ip_is_authorized, key_is_authorized, AuthorizedRequest,
};
use super::errors::FrontendResult;
use axum::headers::{authorization::Bearer, Authorization, Origin, Referer, UserAgent};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::Path,
    response::{IntoResponse, Redirect},
    Extension, TypedHeader,
};
use axum_client_ip::ClientIp;
use axum_macros::debug_handler;
use futures::SinkExt;
use futures::{
    future::AbortHandle,
    stream::{SplitSink, SplitStream, StreamExt},
};
use handlebars::Handlebars;
use hashbrown::HashMap;
use serde_json::{json, value::RawValue};
use std::sync::Arc;
use std::{str::from_utf8_mut, sync::atomic::AtomicUsize};
use tracing::{error, error_span, info, trace, Instrument};

use crate::{
    app::Web3ProxyApp,
    jsonrpc::{JsonRpcForwardedResponse, JsonRpcForwardedResponseEnum, JsonRpcRequest},
};

#[debug_handler]
pub async fn websocket_handler(
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    let request_span = error_span!("request", %ip, ?referer, ?user_agent);

    let (authorized_request, _semaphore) = if let Some(TypedHeader(Authorization(bearer))) = bearer
    {
        let origin = origin.map(|x| x.0);
        let referer = referer.map(|x| x.0);
        let user_agent = user_agent.map(|x| x.0);

        bearer_is_authorized(&app, bearer, ip, origin, referer, user_agent)
            .instrument(request_span.clone())
            .await?
    } else {
        ip_is_authorized(&app, ip)
            .instrument(request_span.clone())
            .await?
    };

    let request_span = error_span!("request", ?authorized_request);

    let authorized_request = Arc::new(authorized_request);

    match ws_upgrade {
        Some(ws) => Ok(ws
            .on_upgrade(|socket| {
                proxy_web3_socket(app, authorized_request, socket).instrument(request_span)
            })
            .into_response()),
        None => {
            // this is not a websocket. redirect to a friendly page
            Ok(Redirect::to(&app.config.redirect_public_url).into_response())
        }
    }
}

#[debug_handler]
pub async fn websocket_handler_with_key(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    Path(user_key): Path<String>,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    let user_key = user_key.parse()?;

    let request_span = error_span!("request", %ip, ?referer, ?user_agent);

    let (authorized_request, _semaphore) = key_is_authorized(
        &app,
        user_key,
        ip,
        origin.map(|x| x.0),
        referer.map(|x| x.0),
        user_agent.map(|x| x.0),
    )
    .instrument(request_span.clone())
    .await?;

    // TODO: type that wraps Address and have it censor? would protect us from accidently logging addresses or other user info
    let request_span = error_span!("request", ?authorized_request);

    let authorized_request = Arc::new(authorized_request);

    match ws_upgrade {
        Some(ws_upgrade) => Ok(ws_upgrade.on_upgrade(move |socket| {
            proxy_web3_socket(app, authorized_request, socket).instrument(request_span)
        })),
        None => {
            // TODO: store this on the app and use register_template?
            let reg = Handlebars::new();

            // TODO: show the user's address, not their id (remember to update the checks for {{user_id}}} in app.rs)
            // TODO: query to get the user's address. expose that instead of user_id
            let user_url = reg
                .render_template(
                    &app.config.redirect_user_url,
                    &json!({ "authorized_request": authorized_request }),
                )
                .unwrap();

            // this is not a websocket. redirect to a page for this user
            Ok(Redirect::to(&user_url).into_response())
        }
    }
}

async fn proxy_web3_socket(
    app: Arc<Web3ProxyApp>,
    authorized_request: Arc<AuthorizedRequest>,
    socket: WebSocket,
) {
    // split the websocket so we can read and write concurrently
    let (ws_tx, ws_rx) = socket.split();

    // create a channel for our reader and writer can communicate. todo: benchmark different channels
    let (response_sender, response_receiver) = flume::unbounded::<Message>();

    tokio::spawn(write_web3_socket(response_receiver, ws_tx));
    tokio::spawn(read_web3_socket(
        app,
        authorized_request,
        ws_rx,
        response_sender,
    ));
}

/// websockets support a few more methods than http clients
async fn handle_socket_payload(
    app: Arc<Web3ProxyApp>,
    authorized_request: Arc<AuthorizedRequest>,
    payload: &str,
    response_sender: &flume::Sender<Message>,
    subscription_count: &AtomicUsize,
    subscriptions: &mut HashMap<String, AbortHandle>,
) -> Message {
    // TODO: do any clients send batches over websockets?
    let (id, response) = match serde_json::from_str::<JsonRpcRequest>(payload) {
        Ok(payload) => {
            // TODO: should we use this id for the subscription id? it should be unique and means we dont need an atomic
            let id = payload.id.clone();

            let response: anyhow::Result<JsonRpcForwardedResponseEnum> = match &payload.method[..] {
                "eth_subscribe" => {
                    // TODO: what should go in this span?
                    let span = error_span!("eth_subscribe");

                    let response = app
                        .eth_subscribe(
                            authorized_request.clone(),
                            payload,
                            subscription_count,
                            response_sender.clone(),
                        )
                        .instrument(span)
                        .await;

                    match response {
                        Ok((handle, response)) => {
                            // TODO: better key
                            subscriptions
                                .insert(response.result.as_ref().unwrap().to_string(), handle);

                            Ok(response.into())
                        }
                        Err(err) => Err(err),
                    }
                }
                "eth_unsubscribe" => {
                    // TODO: how should handle rate limits and stats on this?

                    let subscription_id = payload.params.unwrap().to_string();

                    let partial_response = match subscriptions.remove(&subscription_id) {
                        None => false,
                        Some(handle) => {
                            handle.abort();
                            true
                        }
                    };

                    let response =
                        JsonRpcForwardedResponse::from_value(json!(partial_response), id.clone());

                    Ok(response.into())
                }
                _ => {
                    app.proxy_web3_rpc(&authorized_request, payload.into())
                        .await
                }
            };

            (id, response)
        }
        Err(err) => {
            let id = RawValue::from_string("null".to_string()).unwrap();
            (id, Err(err.into()))
        }
    };

    let response_str = match response {
        Ok(x) => serde_json::to_string(&x),
        Err(err) => {
            // we have an anyhow error. turn it into
            let response = JsonRpcForwardedResponse::from_anyhow_error(err, None, Some(id));
            serde_json::to_string(&response)
        }
    }
    .unwrap();

    Message::Text(response_str)
}

async fn read_web3_socket(
    app: Arc<Web3ProxyApp>,
    authorized_request: Arc<AuthorizedRequest>,
    mut ws_rx: SplitStream<WebSocket>,
    response_sender: flume::Sender<Message>,
) {
    let mut subscriptions = HashMap::new();
    let subscription_count = AtomicUsize::new(1);

    while let Some(Ok(msg)) = ws_rx.next().await {
        // new message from our client. forward to a backend and then send it through response_tx
        let response_msg = match msg {
            Message::Text(payload) => {
                handle_socket_payload(
                    app.clone(),
                    authorized_request.clone(),
                    &payload,
                    &response_sender,
                    &subscription_count,
                    &mut subscriptions,
                )
                .await
            }
            Message::Ping(x) => Message::Pong(x),
            Message::Pong(x) => {
                trace!("pong: {:?}", x);
                continue;
            }
            Message::Close(_) => {
                info!("closing websocket connection");
                break;
            }
            Message::Binary(mut payload) => {
                // TODO: poke rate limit for the user/ip
                let payload = from_utf8_mut(&mut payload).unwrap();

                handle_socket_payload(
                    app.clone(),
                    authorized_request.clone(),
                    payload,
                    &response_sender,
                    &subscription_count,
                    &mut subscriptions,
                )
                .await
            }
        };

        match response_sender.send_async(response_msg).await {
            Ok(_) => {}
            Err(err) => {
                error!("{}", err);
                break;
            }
        };
    }
}

async fn write_web3_socket(
    response_rx: flume::Receiver<Message>,
    mut ws_tx: SplitSink<WebSocket, Message>,
) {
    // TODO: increment counter for open websockets

    while let Ok(msg) = response_rx.recv_async().await {
        // a response is ready

        // TODO: poke rate limits for this user?

        // forward the response to through the websocket
        if let Err(err) = ws_tx.send(msg).await {
            // this isn't a problem. this is common and happens whenever a client disconnects
            trace!(?err, "unable to write to websocket");
            break;
        };
    }

    // TODO: decrement counter for open websockets
}
