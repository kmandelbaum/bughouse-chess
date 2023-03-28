// Improvement potential. Try to do everything via message-passing, without `Mutex`es,
//   but also witout threading and network logic inside `ServerState`.
//   Problem. Adding client via event is a potential race condition in case the
//   first TCP message from the client arrives earlier.

use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use async_tungstenite::WebSocketStream;
use futures_io::{AsyncRead, AsyncWrite};
use futures_util::StreamExt;
use log::{error, info, warn};
use rand::RngCore;
use tide::StatusCode;
use tungstenite::protocol;

use bughouse_chess::server_hooks::ServerHooks;
use bughouse_chess::session_store::*;
use bughouse_chess::{server::*, BughouseServerEvent};

use crate::auth;
use crate::auth_handlers_tide::*;
use crate::database;
use crate::database_server_hooks::*;
use crate::http_server_state::*;
use crate::network::{self, CommunicationError};
use crate::persistence::DatabaseReader;
use crate::secret_persistence::SecretDatabaseRW;
use crate::prod_server_helpers::ProdServerHelpers;
use crate::server_main::{AuthOptions, DatabaseOptions, ServerConfig, SessionOptions};

async fn handle_connection<DB: Sync + Send + 'static, S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static>(
    peer_addr: String,
    stream: WebSocketStream<S>,
    tx: mpsc::SyncSender<IncomingEvent>,
    clients: Arc<Mutex<Clients>>,
    session_id: Option<SessionId>,
    http_server_state: HttpServerState<DB>,
) -> tide::Result<()> {
    let (mut stream_tx, mut stream_rx) = stream.split();
    info!("Client connected: {}", peer_addr);

    let (client_tx, client_rx) = mpsc::channel();

    let session_store_subscription_id = session_id.as_ref().map(|session_id| {
        // Subscribe the client to all updates to the session in session store.
        let my_client_tx = client_tx.clone();
        http_server_state
            .session_store
            .lock()
            .unwrap()
            .subscribe(&session_id, move |s| {
                // Send the entire session data to the client.
                // We can perform some mapping here if we want to hide
                // some of the state from the client.
                let _ =
                    my_client_tx.send(BughouseServerEvent::UpdateSession { session: s.clone() });
            })
    });

    let client_id = clients
        .lock()
        .unwrap()
        .add_client(client_tx, session_id.clone(), peer_addr.to_string());
    let clients_remover = Arc::clone(&clients);
    let remove_client1 = move || {
        match (session_id, session_store_subscription_id) {
            (Some(session_id), Some(session_store_subscription_id)) => {
                http_server_state
                    .session_store
                    .lock()
                    .unwrap()
                    .unsubscribe(&session_id, session_store_subscription_id);
            },
            _ => {},
        };
        clients_remover.lock().unwrap().remove_client(client_id)
    };
    let remove_client2 = remove_client1.clone();

    async_std::task::spawn(async move {
        loop {
            match network::read_obj_async(&mut stream_rx).await {
                Ok(ev) => {
                    tx.send(IncomingEvent::Network(client_id, ev)).unwrap();
                }
                Err(err) => {
                    if let Some(logging_id) = remove_client1() {
                        match err {
                            CommunicationError::ConnectionClosed => {
                                info!("Client {} disconnected", logging_id)
                            }
                            err => warn!(
                                "Client {} disconnected due to read error: {:?}",
                                logging_id, err
                            ),
                        }
                    }
                    break;
                }
            }
        }
    });

    // Still spawning an OS thread here because client_rx is a
    // synchronous receiver.
    // Calling blocking functions (such as client_rx.recv()) within async context
    // means completely blocking an executor thread, which quickly leads to
    // stavation and deadlocks because the number of async executor threads
    // is limited.
    let (done_tx, done_rx) = async_std::channel::bounded(1);
    std::thread::spawn(move || {
        loop {
            let Ok(ev) = client_rx.recv() else { break };
            match async_std::task::block_on(network::write_obj_async(&mut stream_tx, &ev)) {
                Ok(()) => {}
                Err(err) => {
                    if let Some(logging_id) = remove_client2() {
                        warn!(
                            "Client {} disconnected due to write error: {:?}",
                            logging_id, err
                        );
                    }
                    break;
                }
            }
        }
        async_std::task::block_on(done_tx.send(())).unwrap();
    });
    // This instead of just running the loop to completion or join() on the
    // thread for the same reason of not blocking the async executor thread.
    done_rx.recv().await.unwrap();
    Ok(())
}

fn run_tide<DB: Sync + Send + 'static + DatabaseReader>(
    config: ServerConfig,
    db: DB,
    secret_db: Box<dyn SecretDatabaseRW>,
    session_store: Arc<Mutex<SessionStore>>,
    clients: Arc<Mutex<Clients>>,
    tx: mpsc::SyncSender<IncomingEvent>,
) {
    let (google_auth, auth_callback_is_https) = match config.auth_options {
        AuthOptions::NoAuth => (None, false),
        AuthOptions::GoogleAuthFromEnv { callback_is_https } => {
            (Some(auth::GoogleAuth::new().unwrap()), callback_is_https)
        }
    };
    let mut app = tide::with_state(Arc::new(HttpServerStateImpl {
        sessions_enabled: config.session_options != SessionOptions::NoSessions,
        google_auth,
        auth_callback_is_https,
        db,
        secret_db,
        static_content_url_prefix: config.static_content_url_prefix,
        session_store,
    }));

    if app.state().sessions_enabled {
        let secret = match config.session_options {
            SessionOptions::NoSessions => unreachable!(),
            SessionOptions::WithNewRandomSecret => {
                let mut result = vec![0u8; 32];
                rand::thread_rng().fill_bytes(result.as_mut_slice());
                result
            }
            SessionOptions::WithSecret(secret) => secret,
        };
        app.with(
            tide::sessions::SessionMiddleware::new(
                tide::sessions::CookieStore::new(),
                secret.as_slice(),
            )
            // Set to Lax so that the session persists third-party
            // redirects like Google Auth.
            .with_same_site_policy(http_types::cookies::SameSite::Lax),
        );
    }

    app.with(tide::utils::After(|mut res: tide::Response| async {
        if let Some(err) = res.error() {
            let msg = format!("Error: {:#?}", err);
            res.set_status(err.status());
            res.set_body(msg);
        }
        Ok(res)
    }));

    app.at(AUTH_SIGNUP_PATH).post(handle_signup);
    app.at(AUTH_LOGIN_PATH).post(handle_login);
    app.at(AUTH_LOGOUT_PATH).post(handle_logout);
    app.at(AUTH_SIGN_WITH_GOOGLE_PATH).get(handle_sign_with_google);
    app.at(AUTH_CONTINUE_SIGN_WITH_GOOGLE_PATH).get(handle_continue_sign_with_google);
    app.at(AUTH_FINISH_SIGNUP_WITH_GOOGLE_PATH).post(handle_finish_signup_with_google);
    app.at(AUTH_CHANGE_ACCOUNT_PATH).post(handle_change_account);
    app.at(AUTH_DELETE_ACCOUNT_PATH).post(handle_delete_account);
    app.at(AUTH_MYSESSION_PATH).get(handle_mysession);

    crate::stats_handlers_tide::Handlers::<HttpServerState<DB>>::register_handlers(&mut app);

    let allowed_origin = config.allowed_origin;

    app.at("/")
        .get(move |req: tide::Request<HttpServerState<_>>| {
            let mytx = tx.clone();
            let myclients = clients.clone();
            let allowed_origin = allowed_origin.clone();
            async move {
                if req.state().sessions_enabled {
                    // When the sessions are enabled, we might be using the session
                    // cookie for authentication.
                    // We should be checking request origin in that case to
                    // preven CSRF, to which websockets are inherently vulnerable.
                    check_origin(&req, &allowed_origin)?;
                }
                let peer_addr = req.peer_addr().map_or_else(
                    || {
                        Err(tide::Error::from_str(
                            StatusCode::Forbidden,
                            "Peer address missing",
                        ))
                    },
                    |x| Ok(x.to_owned()),
                )?;

                let session_id = get_session_id(&req).ok();

                let http_server_state = req.state().clone();
                // tide::Request -> http_types::Request -> http::Request<Body> -> http::Request<()>.
                let http_types_req: http_types::Request = req.into();
                let http_req_with_body: http::Request<http_types::Body> = http_types_req.into();
                let http_req = http_req_with_body.map(|_| ());

                let http_resp = tungstenite::handshake::server::create_response(&http_req)
                    .map_err(|e| tide::Error::new(StatusCode::BadRequest, e))?;

                // http::Response<()> -> http::Response<Body> -> http_types::Response
                let http_resp_with_body = http_resp.map(|_| http_types::Body::empty());
                let mut http_types_resp: http_types::Response = http_resp_with_body.into();

                // http_types::Response is a magic thing that can give us the stream back
                // once it's upgraded.
                let upgrade_receiver = http_types_resp.recv_upgrade().await;

                async_std::task::spawn(async move {
                    if let Some(stream) = upgrade_receiver.await {
                        let stream =
                            WebSocketStream::from_raw_socket(stream, protocol::Role::Server, None)
                                .await;
                        if let Err(err) = handle_connection(
                            peer_addr,
                            stream,
                            mytx,
                            myclients,
                            session_id,
                            http_server_state,
                        )
                        .await
                        {
                            error!("{}", err);
                        }
                    } else {
                        error!("Never received an upgrade for client {}", peer_addr);
                    }
                });
                Ok(http_types_resp)
            }
        });
    async_std::task::block_on(async { app.listen(format!("0.0.0.0:{}", network::PORT)).await })
        .expect("Failed to start the tide server");
}

pub fn run(config: ServerConfig) {
    assert!(
        config.auth_options == AuthOptions::NoAuth
            || config.session_options != SessionOptions::NoSessions,
        "Authentication is enabled while sessions are not."
    );

    // Limited buffer for data streaming from clients into the server.
    // When this is full because ServerState::apply_event isn't coping with
    // the load, we start putting back pressure on client websockets.
    let (tx, rx) = mpsc::sync_channel(100000);
    let tx_tick = tx.clone();
    let tx_terminate = tx.clone();
    let clients = Arc::new(Mutex::new(Clients::new()));
    let clients_copy = Arc::clone(&clients);

    ctrlc::set_handler(move || tx_terminate.send(IncomingEvent::Terminate).unwrap())
        .expect("Error setting Ctrl-C handler");

    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(100));
        tx_tick.send(IncomingEvent::Tick).unwrap();
    });

    let hooks = match config.database_options.clone() {
        DatabaseOptions::NoDatabase => None,
        DatabaseOptions::Sqlite(address) => {
            let db = database::SqlxDatabase::<sqlx::Sqlite>::new(&address)
                .expect(format!("Cannot connect to SQLite DB {address}").as_str());
            let h = DatabaseServerHooks::new(db).expect("Cannot initialize hooks");
            Some(Box::new(h) as Box<dyn ServerHooks + Send>)
        }

        DatabaseOptions::Postgres(address) => {
            let db = database::SqlxDatabase::<sqlx::Postgres>::new(&address)
                .expect(format!("Cannot connect to Postgres DB {address}").as_str());
            let h = DatabaseServerHooks::new(db).expect("Cannot initialize hooks");
            Some(Box::new(h) as Box<dyn ServerHooks + Send>)
        }
    };

    let secret_database = make_database(&config.secret_database_options).unwrap();
    let _ = async_std::task::block_on(secret_database.create_tables()).map_err(|err| {
        error!("Failed to create tables: {}", err);
        // Proceed even if table creation failed.
    });

    let session_store = Arc::new(Mutex::new(SessionStore::new()));
    let session_store_copy = Arc::clone(&session_store);

    thread::spawn(move || {
        let mut server_state = ServerState::new(
            clients_copy,
            session_store_copy,
            Box::new(ProdServerHelpers {}),
            hooks.map(|h| h as Box<dyn ServerHooks>),
        );

        for event in rx {
            server_state.apply_event(event);
        }
        panic!("Unexpected end of events stream");
    });

    match config.database_options.clone() {
        DatabaseOptions::NoDatabase => run_tide(
            config,
            database::UnimplementedDatabase {},
            secret_database,
            session_store,
            clients,
            tx,
        ),
        DatabaseOptions::Sqlite(address) => run_tide(
            config,
            database::SqlxDatabase::<sqlx::Sqlite>::new(&address).unwrap(),
            secret_database,
            session_store,
            clients,
            tx,
        ),
        DatabaseOptions::Postgres(address) => run_tide(
            config,
            database::SqlxDatabase::<sqlx::Postgres>::new(&address).unwrap(),
            secret_database,
            session_store,
            clients,
            tx,
        ),
    }
}

fn make_database(options: &DatabaseOptions) -> anyhow::Result<Box<dyn SecretDatabaseRW>> {
    Ok(match options {
        DatabaseOptions::NoDatabase => Box::new(database::UnimplementedDatabase {}),
        DatabaseOptions::Sqlite(address) => {
            Box::new(database::SqlxDatabase::<sqlx::Sqlite>::new(&address)?)
        }
        DatabaseOptions::Postgres(address) => {
            Box::new(database::SqlxDatabase::<sqlx::Postgres>::new(&address)?)
        }
    })
}
