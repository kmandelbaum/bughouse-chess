// Improvement potential. Try to do everything via message-passing, without `Mutex`es,
//   but also witout threading and network logic inside `ServerState`.
//   Problem. Adding client via event is a potential race condition in case the
//   first TCP message from the client arrives earlier.

use std::net::{TcpStream, TcpListener};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use log::{info, warn};
use tungstenite::http::Request;
use tungstenite::protocol;

use bughouse_chess::*;
use bughouse_chess::server::*;
use bughouse_chess::server_hooks::ServerHooks;

use crate::network::{self, CommunicationError};
use crate::sqlx_server_hooks::*;

pub enum DatabaseOptions {
    NoDatabase,
    Sqlite(String),
    Postgres(String),
}

pub struct ServerConfig {
    pub database_options: DatabaseOptions,
}

fn to_debug_string<T: std::fmt::Debug>(v: T) -> String {
    format!("{v:?}")
}

// The secret key would usually be read from a configuration file/environment variables.
fn get_secret_key() -> cookie::Key {
    cookie::Key::from("ahY8aec0mohjei8nu7bu4ohpuowooqu7uam0chaiquechoo1ii7aedoh4quouru3ziPeehiujethangath9kahphoo0kohhuewaew2ODohgie1Teuxe0Ziemacei1ya6eemeis9laay7WooyaeK8Eed7tai7Doh5eiChieshiep7aequoraceiquoY1yuh9iefeecae9vioc8xee4quei6aiN2oobiile6aemoo3suT8Ahyi8kee0oht7daewud6OoKueg3aebahWahgahheiYai0hai1thaePhae0aet8aequee0aiGhohn0gaizoogh9vohV5ceebu7aeheecoo6ingaikozahnaihaey8ohjad2aetoa7voothi3ieS2KeirachieQu4vih8tah4xeong8oufeife7eeveem1yooqu4rai9eenool0xohbiu7Eex2aiP1Aeh2koh8xiecooriohuu8miephohp5fiengaeX8zanu7Ith3ooyaeNg9".as_bytes())
}

fn get_session_or_log<T>(req: &Request<T>) -> Option<String> {
    let all_cookies = req.headers().get_all(http::header::COOKIE);
    info!("{:?}", all_cookies.into_iter().collect::<Vec<_>>());
    let all_cookies = req.headers().get(http::header::COOKIE)?;
    info!("Header {:?}", all_cookies);
    let cookie_header_str = match all_cookies.to_str() {
        Ok(c) => c,
        Err(e) => {
            log::error!("failed to get cookie from header: {e}");
            return None;
        }
    };
    info!("Header {:?}", cookie_header_str);
    let mut jar = cookie::CookieJar::new();
    for cookie_piece in cookie_header_str.split("; ") { 
        match cookie::Cookie::parse(cookie_piece) {
            Ok(c) => {
                info!("Cookie {:?}", c);
                if c.name() == "session" {
                    jar.add(c.into_owned());
                    let private_jar = jar.private(&get_secret_key());
                    return private_jar.get("session").map(|v| v.value().to_owned());
                }
            }
            Err(e) => {
                log::error!("failed to parse cookie: {e}");
                continue;
            }
        }
    }
    None
}

fn handle_connection(stream: TcpStream, clients: &Arc<Mutex<Clients>>, tx: mpsc::Sender<IncomingEvent>)
    -> Result<(), String>
{
    let peer_addr = stream.peer_addr().map_err(to_debug_string)?;
    info!("Client connected: {}", peer_addr);
    let mut maybe_session: Option<String> = None;
    //let mut socket_in = tungstenite::accept(stream).map_err(to_debug_string)?;
    let mut socket_in = tungstenite::accept_hdr(stream, |req: &Request<_>, resp| {
        // TODO: !!! Chech the 'Origin' header to prevent CSRF.
        maybe_session = get_session_or_log(req);
        Ok(resp)
    }).map_err(|e| format!("{}", e))?;

    info!("Accepted {}; cookie: {:?}", peer_addr, maybe_session);

    let mut socket_out = network::clone_websocket(&socket_in, protocol::Role::Server);
    let (client_tx, client_rx) = mpsc::channel();
    let client_id = clients.lock().unwrap().add_client(client_tx, peer_addr.to_string());
    let clients_remover1 = Arc::clone(&clients);
    let clients_remover2 = Arc::clone(&clients);
    // Rust-upgrade (https://github.com/rust-lang/rust/issues/90470):
    //   Use `JoinHandle.is_running` in order to join the read/write threads in a
    //   non-blocking way.
    thread::spawn(move || {
        loop {
            match network::read_obj(&mut socket_in) {
                Ok(ev) => {
                    tx.send(IncomingEvent::Network(client_id, ev)).unwrap();
                },
                Err(err) => {
                    if let Some(logging_id) = clients_remover1.lock().unwrap().remove_client(client_id) {
                        match err {
                            CommunicationError::ConnectionClosed => info!("Client {} disconnected", logging_id),
                            err => warn!("Client {} disconnected due to read error: {:?}", logging_id, err),
                        }
                    }
                    return;
                },
            }
        }
    });
    thread::spawn(move || {
        for ev in client_rx {
            match network::write_obj(&mut socket_out, &ev) {
                Ok(()) => {},
                Err(err) => {
                    if let Some(logging_id) = clients_remover2.lock().unwrap().remove_client(client_id) {
                        warn!("Client {} disconnected due to write error: {:?}", logging_id, err);
                    }
                    return;
                },
            }
        }
    });
    Ok(())
}

pub fn run(config: ServerConfig) {
    let (tx, rx) = mpsc::channel();
    let tx_tick = tx.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_millis(100));
            tx_tick.send(IncomingEvent::Tick).unwrap();
        }
    });
    let clients = Arc::new(Mutex::new(Clients::new()));
    let clients_copy = Arc::clone(&clients);

    thread::spawn(move || {
        let hooks = match config.database_options {
            DatabaseOptions::NoDatabase => None,
            DatabaseOptions::Sqlite(address) =>
                Some(Box::new(
                    SqlxServerHooks::<sqlx::Sqlite>::new(address.as_str()).unwrap_or_else(
                            |err| panic!("Cannot connect to SQLite DB {address}:\n{err}")))
                    as Box<dyn ServerHooks>
                ),
            DatabaseOptions::Postgres(address) =>
                Some(Box::new(
                    SqlxServerHooks::<sqlx::Postgres>::new(address.as_str()).unwrap_or_else(
                            |err| panic!("Cannot connect to Postgres DB {address}:\n{err}")))
                    as Box<dyn ServerHooks>
                ),
        };
        let mut server_state = ServerState::new(
            clients_copy,
            hooks
        );

        for event in rx {
            server_state.apply_event(event);
        }
        panic!("Unexpected end of events stream");
    });

    let listener = TcpListener::bind(("0.0.0.0", network::PORT)).unwrap();
    info!("Starting bughouse server version {}...", my_git_version!());
    info!("Listening to connections on {}...", listener.local_addr().unwrap());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                match handle_connection(stream, &clients, tx.clone()) {
                    Ok(()) => {},
                    Err(err) => {
                        warn!("{}", err);
                    }
                }
            }
            Err(err) => {
                warn!("Cannot establish connection: {}", err);
            }
        }
    }
    panic!("Unexpected end of TcpListener::incoming");
}
