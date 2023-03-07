// Improvement potential. Replace `game.find_player(&self.players[participant_id].name)`
//   with a direct mapping (participant_id -> player_bughouse_id).

use std::collections::{HashSet, HashMap, hash_map, BTreeMap};
use std::cmp;
use std::iter;
use std::ops;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::Duration;

use enum_map::enum_map;
use indoc::printdoc;
use instant::Instant;
use itertools::Itertools;
use log::{info, warn};
use rand::{Rng, seq::SliceRandom};
use strum::IntoEnumIterator;

use crate::board::{TurnMode, TurnError, TurnInput, VictoryReason};
use crate::chalk::{ChalkDrawing, Chalkboard};
use crate::clock::GameInstant;
use crate::game::{
    TurnRecord, BughouseBoard, BughouseEnvoy, BughousePlayer, PlayerInGame, BughouseGameStatus, BughouseGame,
    get_bughouse_force, TOTAL_ENVOYS, TOTAL_ENVOYS_PER_TEAM,
};
use crate::event::{BughouseServerEvent, BughouseClientEvent, BughouseServerRejection, BughouseClientErrorReport};
use crate::lobby::{num_fixed_players_per_team, verify_participants};
use crate::pgn::{self, BughouseExportFormat};
use crate::ping_pong::{PassiveConnectionMonitor, PassiveConnectionStatus};
use crate::player::{Participant, Team, Faction};
use crate::rules::{Teaming, Rules, FIRST_GAME_COUNTDOWN_DURATION};
use crate::scores::Scores;
use crate::server_helpers::ServerHelpers;
use crate::server_hooks::{ServerHooks, NoopServerHooks};


const DOUBLE_TERMINATION_ABORT_THRESHOLD: Duration = Duration::from_secs(1);
const TERMINATION_WAITING_PERIOD: Duration = Duration::from_secs(60);
const CONTEST_GC_INACTIVITY_THRESHOLD: Duration = Duration::from_secs(3600 * 24);

macro_rules! unknown_error {
    ($($arg:tt)*) => {
        BughouseServerRejection::UnknownError{ message: format!($($arg)*) }
    }
}

type Preturns = HashMap<BughouseEnvoy, TurnInput>;

#[derive(Clone, Copy, Debug)]
enum Execution {
    // The server runs normally.
    Running,

    // The server is in graceful shutdown mode. It will not allow to start new contests or
    // new games within existing contests and it will automatically shut down when there are
    // no more games running.
    ShuttingDown {
        // The moment shutdown was requested initially.
        shutting_down_since: Instant,
        // Last termination request (i.e. last time Ctrl+C was pressed). The server will abort
        // upon two termination requests come within `DOUBLE_TERMINATION_ABORT_THRESHOLD` period.
        last_termination_request: Instant,
    },
}

#[derive(Debug)]
pub enum IncomingEvent {
    Network(ClientId, BughouseClientEvent),
    Tick,
    Terminate,
}

#[derive(Debug)]
pub struct GameState {
    game: BughouseGame,
    // `game_creation` is the time when seating and starting position were
    // generated and presented to the users.
    game_creation: Instant,
    // `game_start` is the time when the clock started after a player made their
    // first turn. We need both an Instant and an OffsetDateTime: the instant
    // time is used for monotonic in-game time tracking, and the offset time is
    // used for communication with outside world about absolute moments in time.
    game_start: Option<Instant>,
    game_start_offset_time: Option<time::OffsetDateTime>,
    game_end: Option<Instant>,
    preturns: Preturns,
    chalkboard: Chalkboard,
}

impl GameState {
    pub fn game(&self) -> &BughouseGame { &self.game }
    pub fn rated(&self) -> bool { self.game.contest_rules().rated }
    pub fn start_offset_time(&self) -> Option<time::OffsetDateTime> {
        self.game_start_offset_time
    }
}


#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct ContestId(String);


#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
struct ParticipantId(usize);

#[derive(Debug)]
struct Participants {
    // Use an ordered map to show lobby players in joining order.
    map: BTreeMap<ParticipantId, Participant>,
    next_id: usize,
}

impl Participants {
    fn new() -> Self { Self{ map: BTreeMap::new(), next_id: 1 } }
    fn iter(&self) -> impl Iterator<Item = &Participant> + Clone { self.map.values() }
    fn iter_mut(&mut self) -> impl Iterator<Item = &mut Participant> { self.map.values_mut() }
    fn find_by_name(&self, name: &str) -> Option<ParticipantId> {
        self.map.iter().find_map(|(id, p)| if p.name == name { Some(*id) } else { None })
    }
    fn add_participant(&mut self, participant: Participant) -> ParticipantId {
        let id = ParticipantId(self.next_id);
        self.next_id += 1;
        assert!(self.map.insert(id, participant).is_none());
        id
    }
}

impl ops::Index<ParticipantId> for Participants {
    type Output = Participant;
    fn index(&self, id: ParticipantId) -> &Self::Output { &self.map[&id] }
}
impl ops::IndexMut<ParticipantId> for Participants {
    fn index_mut(&mut self, id: ParticipantId) -> &mut Self::Output { self.map.get_mut(&id).unwrap() }
}


#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ClientId(usize);

pub struct Client {
    events_tx: mpsc::Sender<BughouseServerEvent>,
    contest_id: Option<ContestId>,
    participant_id: Option<ParticipantId>,
    logging_id: String,
    connection_monitor: PassiveConnectionMonitor,
}

impl Client {
    fn send(&mut self, event: BughouseServerEvent) {
        self.events_tx.send(event).unwrap();
    }
    fn send_rejection(&mut self, rejection: BughouseServerRejection) {
        self.send(BughouseServerEvent::Rejection(rejection));
    }
}

pub struct Clients {
    map: HashMap<ClientId, Client>,
    next_id: usize,
}

impl Clients {
    pub fn new() -> Self { Clients{ map: HashMap::new(), next_id: 1 } }

    pub fn add_client(&mut self, events_tx: mpsc::Sender<BughouseServerEvent>, logging_id: String)
        -> ClientId
    {
        let now = Instant::now();
        let client = Client {
            events_tx,
            contest_id: None,
            participant_id: None,
            logging_id,
            connection_monitor: PassiveConnectionMonitor::new(now),
        };
        let id = ClientId(self.next_id);
        self.next_id += 1;
        assert!(self.map.insert(id, client).is_none());
        id
    }

    // Returns `logging_id` if the client existed.
    // A client can be removed multiple times, e.g. first on `Leave`, then on network
    // channel closure. This is not an error.
    //
    // TODO: Make sure network connection is closed in a reasnable timeframe whenever
    //   a client is removed.
    //
    // Improvement potential. Send an event informing other clients that somebody went
    // offline (for TUI: could use “ϟ” for “disconnected”; there is a plug emoji “🔌”
    // that works much better, but it's not supported by console fonts).
    pub fn remove_client(&mut self, id: ClientId) -> Option<String> {
        self.map.remove(&id).map(|client| client.logging_id)
    }

    // Sends the event to each client who has joined the contest.
    //
    // Improvement potential. Do not iterate over all clients. Keep the list of clients
    // in each contest.
    fn broadcast(&mut self, contest_id: &ContestId, event: &BughouseServerEvent) {
        for client in self.map.values_mut() {
            if client.contest_id.as_ref() == Some(contest_id) {
                client.send(event.clone());
            }
        }
    }

    fn find_participant(&self, participant_id: ParticipantId) -> Option<ClientId> {
        self.map.iter().find_map(
            |(&id, c)| if c.participant_id == Some(participant_id) { Some(id) } else { None }
        )
    }
}

impl ops::Index<ClientId> for Clients {
    type Output = Client;
    fn index(&self, id: ClientId) -> &Self::Output { &self.map[&id] }
}
impl ops::IndexMut<ClientId> for Clients {
    fn index_mut(&mut self, id: ClientId) -> &mut Self::Output { self.map.get_mut(&id).unwrap() }
}


#[derive(Clone, PartialEq, Eq, Debug)]
enum ContestActivity {
    Present,
    Past(Instant),
}

#[derive(Debug)]
struct Contest {
    contest_id: ContestId,
    contest_creation: Instant,
    rules: Rules,
    participants: Participants,
    scores: Scores,
    match_history: Vec<BughouseGame>,  // final game states
    first_game_countdown_since: Option<Instant>,
    game_state: Option<GameState>,  // active game or latest game
    board_assignment_override: Option<Vec<PlayerInGame>>,  // for tests
}

struct Context<'a, 'b> {
    clients: &'b mut MutexGuard<'a, Clients>,
    helpers: &'a mut dyn ServerHelpers,
    hooks: &'a mut dyn ServerHooks,
    disable_countdown: bool,
}

struct CoreServerState {
    execution: Execution,
    contests: HashMap<ContestId, Contest>,
}

type EventResult = Result<(), BughouseServerRejection>;

// Split state into two parts (core and context) in order to allow things like:
//   let mut clients = self.clients.lock().unwrap();
//   self.core.foo(&mut clients);
// whereas
//   self.foo(&mut clients);
// would make the compiler complain that `self` is borrowed twice.
pub struct ServerState {
    // Optimization potential: Lock-free map instead of Mutex<HashMap>.
    clients: Arc<Mutex<Clients>>,
    helpers: Box<dyn ServerHelpers>,
    hooks: Box<dyn ServerHooks>,
    // TODO: Remove special test paths, use proper mock clock instead.
    disable_countdown: bool,  // for tests
    core: CoreServerState,
}

impl ServerState {
    pub fn new(
        clients: Arc<Mutex<Clients>>,
        helpers: Box<dyn ServerHelpers>,
        hooks: Option<Box<dyn ServerHooks>>,
    ) -> Self {
        ServerState {
            clients,
            helpers,
            hooks: hooks.unwrap_or_else(|| Box::new(NoopServerHooks{})),
            disable_countdown: false,
            core: CoreServerState::new(),
        }
    }

    pub fn apply_event(&mut self, event: IncomingEvent) {
        // Lock clients for the entire duration of the function. This means simpler and
        // more predictable event processing, e.g. it gives a guarantee that all broadcasts
        // from a single `apply_event` reach the same set of clients.
        //
        // Improvement potential. Rethink this approach. With multiple parallel contests this
        // global mutex may become a bottleneck.
        let mut clients = self.clients.lock().unwrap();

        // Improvement potential. Consider adding commonly used things like `now` and `execution`
        // to `Context`.
        let mut ctx = Context {
            clients: &mut clients,
            helpers: self.helpers.as_mut(),
            hooks: self.hooks.as_mut(),
            disable_countdown: self.disable_countdown,
        };

        self.core.apply_event(&mut ctx, event);
    }

    #[allow(non_snake_case)]
    pub fn TEST_disable_countdown(&mut self) {
        self.disable_countdown = true;
    }

    #[allow(non_snake_case)]
    pub fn TEST_override_board_assignment(&mut self, contest_id: String, assignment: Vec<PlayerInGame>) {
        let contest_id = ContestId(contest_id);
        self.core.contests.get_mut(&contest_id).unwrap().board_assignment_override = Some(assignment);
    }
}

impl CoreServerState {
    fn new() -> Self {
        CoreServerState {
            execution: Execution::Running,
            contests: HashMap::new(),
        }
    }

    fn make_contest(&mut self, now: Instant, rules: Rules) -> Result<ContestId, String> {
        rules.verify().map_err(|err| format!("Invalid contest rules: {err}"))?;
        // Exclude confusing characters:
        //   - 'O' and '0' (easy to confuse);
        //   - 'I' (looks like '1'; keep '1' because confusion in the other direction seems less likely).
        const ALPHABET: [char; 33] = [
            '1', '2', '3', '4', '5', '6', '7', '8', '9',
            'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'J', 'K', 'L', 'M',
            'N', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
        ];
        const MIN_ID_LEN: usize = 4;
        const MAX_ATTEMPTS_PER_LEN: usize = 100;
        let mut rng = rand::thread_rng();
        let mut id_len = MIN_ID_LEN;
        let mut id = ContestId(String::new());
        let mut attempts_at_this_len = 0;
        while id.0.is_empty() || self.contests.contains_key(&id) {
            id = ContestId((&mut rng)
                .sample_iter(rand::distributions::Uniform::from(0..ALPHABET.len()))
                .map(|idx| ALPHABET[idx])
                .take(id_len)
                .collect()
            );
            attempts_at_this_len += 1;
            if attempts_at_this_len > MAX_ATTEMPTS_PER_LEN {
                id_len += 1;
                attempts_at_this_len = 0;
            }
        }
        // TODO: Verify that time limit is not too large: the contest will not be GCed while the
        //   clock's ticking even if all players left.
        let contest = Contest {
            contest_id: id.clone(),
            contest_creation: now,
            rules,
            participants: Participants::new(),
            scores: Scores::new(),
            match_history: Vec::new(),
            first_game_countdown_since: None,
            game_state: None,
            board_assignment_override: None,
        };
        assert!(self.contests.insert(id.clone(), contest).is_none());
        Ok(id)
    }

    fn apply_event(&mut self, ctx: &mut Context, event: IncomingEvent) {
        // Use the same timestamp for the entire event processing. Other code reachable
        // from this function should not call `Instant::now()`. Doing so may cause a race
        // condition: e.g. if we check the flag, see that it's ok and then continue to
        // write down a turn which, by that time, becomes illegal because player's time
        // is over.
        let now = Instant::now();

        match event {
            IncomingEvent::Network(client_id, event) => self.on_client_event(ctx, client_id, now, event),
            IncomingEvent::Tick => self.on_tick(ctx, now),
            IncomingEvent::Terminate => self.on_terminate(ctx, now),
        }
    }

    fn on_client_event(
        &mut self, ctx: &mut Context, client_id: ClientId, now: Instant, event: BughouseClientEvent
    ) {
        ctx.hooks.on_client_event(&event);

        if !ctx.clients.map.contains_key(&client_id) {
            // TODO: Should there be an exception for `BughouseClientEvent::ReportError`?
            // TODO: Improve logging. Consider:
            //   - include logging_id inside client_id, or
            //   - keep disconnected clients in the map for some time.
            warn!("Got an event from disconnected client:\n{event:?}");
            return;
        }

        ctx.clients[client_id].connection_monitor.register_incoming(now);

        // First, process events that don't require a contest.
        match &event {
            BughouseClientEvent::ReportPerformace(..) => {
                // Only used by server hooks.
                return;
            },
            BughouseClientEvent::ReportError(report) => {
                process_report_error(ctx, client_id, report);
                return;
            },
            BughouseClientEvent::Ping => {
                process_ping(ctx, client_id);
                return;
            },
            _ => {},
        };

        let contest_id = match &event {
            BughouseClientEvent::NewContest{ rules, .. } => {
                if !matches!(self.execution, Execution::Running) {
                    ctx.clients[client_id].send_rejection(BughouseServerRejection::ShuttingDown);
                    return;
                }
                ctx.clients[client_id].contest_id = None;
                ctx.clients[client_id].participant_id = None;
                let contest_id = match self.make_contest(now, rules.clone()) {
                    Ok(id) => id,
                    Err(message) => {
                        ctx.clients[client_id].send_rejection(BughouseServerRejection::UnknownError{ message });
                        return;
                    },
                };
                info!("Contest {} created by client {}", contest_id.0, ctx.clients[client_id].logging_id);
                Some(contest_id)
            },
            BughouseClientEvent::Join{ contest_id, .. } => {
                // Improvement potential: Log cases when a client reconnects to their current
                //   contest. This likely indicates a client error.
                ctx.clients[client_id].contest_id = None;
                ctx.clients[client_id].participant_id = None;
                Some(ContestId(contest_id.clone()))
            },
            _ => ctx.clients[client_id].contest_id.clone(),
        };

        let Some(contest_id) = contest_id else {
            // We've already processed all events that do not depend on a contest.
            ctx.clients[client_id].send_rejection(
                unknown_error!("Cannot process event: no contest in progress")
            );
            return;
        };

        let Some(contest) = self.contests.get_mut(&contest_id) else {
            // The only way to have a contest_id with no contest is when a client is trying
            // to join with a bad contest_id. In other cases we are getting contest_id from
            // trusted internal sources, so the contest must exist as well.
            assert!(matches!(event, BughouseClientEvent::Join{ .. }));
            ctx.clients[client_id].send_rejection(BughouseServerRejection::NoSuchContest {
                contest_id: contest_id.0
            });
            return;
        };

        // Test flags first. Thus we make sure that turns and other actions are
        // not allowed after the time is over.
        contest.test_flags(ctx, now);
        contest.process_client_event(ctx, client_id, self.execution, now, event);
        contest.post_process(ctx, self.execution, now);
    }

    fn on_tick(&mut self, ctx: &mut Context, now: Instant) {
        self.gc_old_contests(now);
        self.check_client_connections(ctx, now);
        for contest in self.contests.values_mut() {
            contest.test_flags(ctx, now);
            contest.post_process(ctx, self.execution, now);
        }
        if !matches!(self.execution, Execution::Running) && self.num_active_contests(now) == 0 {
            println!("No more active contests left. Shutting down.");
            shutdown();
        }
    }

    pub fn on_terminate(&mut self, ctx: &mut Context, now: Instant) {
        const ABORT_INSTRUCTION: &str = "Press Ctrl+C twice within a second to abort immediately.";
        let num_active_contests = self.num_active_contests(now);
        match self.execution {
            Execution::Running => {
                if num_active_contests == 0 {
                    println!("There are no active contests. Shutting down immediately!");
                    shutdown();
                } else {
                    printdoc!("
                        Shutdown requested!
                        The server will not allow to start new contests or games. It will terminate as
                        soon as there are no active contests. There are currently {num_active_contests} active contests.
                        {ABORT_INSTRUCTION}
                    ");
                    self.execution = Execution::ShuttingDown {
                        shutting_down_since: now,
                        last_termination_request: now,
                    };
                    self.contests.values().for_each(|contest| {
                        // Immediately notify clients who ate still in the lobby: they wouldn't be able
                        // to do anything meaningful. Let other players finish their games.
                        //
                        // Improvement potential: Notify everybody immediately, let the clients decide
                        // when it's appropriate to show the message to the user. Pros:
                        //   - Server code will be simpler. There will be exactly two points when a
                        //     shutdown notice should be sent: to all existing clients when termination
                        //     is requested and to new clients as soon as they are connected (to the
                        //     server, not to the contest).
                        //   - Clients will get the relevant information sooner.
                        if contest.game_state.is_none() {
                            contest.broadcast(ctx, &BughouseServerEvent::Rejection(BughouseServerRejection::ShuttingDown));
                        }
                    });
                }
            },
            Execution::ShuttingDown{ shutting_down_since, ref mut last_termination_request } => {
                if now.duration_since(*last_termination_request) <= DOUBLE_TERMINATION_ABORT_THRESHOLD {
                    println!("Aborting!");
                    shutdown();
                } else {
                    let shutdown_duration_sec = now.duration_since(shutting_down_since).as_secs();
                    println!(
                        "Shutdown was requested {}s ago. Waiting for {} active contests to finish.\n{}",
                        shutdown_duration_sec,
                        num_active_contests,
                        ABORT_INSTRUCTION,
                    );
                }
                *last_termination_request = now;
            },
        }
    }

    fn num_active_contests(&self, now: Instant) -> usize {
        self.contests.values().filter(|contest| {
            match contest.latest_activity() {
                ContestActivity::Present => true,
                ContestActivity::Past(t) => now.duration_since(t) <= TERMINATION_WAITING_PERIOD
            }
        }).count()
    }

    fn gc_old_contests(&mut self, now: Instant) {
        // Improvement potential. GC unused contests (zero games and/or no players) sooner.
        self.contests.retain(|_, contest| {
            match contest.latest_activity() {
                ContestActivity::Present => true,
                ContestActivity::Past(t) => now.duration_since(t) <= CONTEST_GC_INACTIVITY_THRESHOLD
            }
        });
    }

    fn check_client_connections(&mut self, ctx: &mut Context, now: Instant) {
        use PassiveConnectionStatus::*;
        ctx.clients.map.retain(|_, client| {
            match client.connection_monitor.status(now) {
                Healthy | TemporaryLost => true,
                PermanentlyLost => false,
            }
        });
    }
}

impl Contest {
    fn latest_activity(&self) -> ContestActivity {
        if let Some(GameState{ game_creation, game_start, game_end, .. }) = self.game_state {
            if let Some(game_end) = game_end {
                ContestActivity::Past(game_end)
            } else if game_start.is_some() {
                ContestActivity::Present
            } else {
                // Since `latest_activity` is used for things like contest GC, we do not want to
                // count the period between game creation and game start as activity:
                //   - In a normal case players will start the game soon, so the contest will not
                //     be GCed;
                //   - In a pathological case the contest could stay in this state indefinitely,
                //     leading to an unrecoverable leak. The only safe time to consider a contest to
                //     be active is when a game is when the game is active and the clock's ticking,
                //     because this period is inherently time-bound.
                ContestActivity::Past(game_creation)
            }
        } else {
            ContestActivity::Past(self.contest_creation)
        }
    }

    fn test_flags(&mut self, ctx: &mut Context, now: Instant) {
        if let Some(GameState{ game_start, ref mut game_end, ref mut game, ref mut preturns, .. }) = self.game_state {
            if let Some(game_start) = game_start {
                if game.status() == BughouseGameStatus::Active {
                    let game_now = GameInstant::from_now_game_active(game_start, now);
                    game.test_flag(game_now);
                    if game.status() != BughouseGameStatus::Active {
                        update_state_on_game_over(game, preturns, &mut self.participants, &mut self.scores, game_end, now);
                        let ev = BughouseServerEvent::GameOver {
                            time: game_now,
                            game_status: game.status(),
                            scores: self.scores.clone(),
                        };
                        self.broadcast(ctx, &ev);
                    }
                }
            }
        }
    }

    fn broadcast(&self, ctx: &mut Context, event: &BughouseServerEvent) {
        ctx.hooks.on_server_broadcast_event(event, self.game_state.as_ref(), self.match_history.len() + 1);
        ctx.clients.broadcast(&self.contest_id, event);
    }

    fn process_client_event(
        &mut self, ctx: &mut Context, client_id: ClientId, execution: Execution, now: Instant, event: BughouseClientEvent
    ) {
        let result = match event {
            BughouseClientEvent::NewContest{ player_name, .. } => {
                // The contest was created earlier.
                self.join_participant(ctx, client_id, execution, now, player_name)
            },
            BughouseClientEvent::Join{ contest_id: _, player_name } => {
                self.join_participant(ctx, client_id, execution, now, player_name)
            },
            BughouseClientEvent::SetFaction{ faction } => {
                self.process_set_faction(ctx, client_id, faction)
            },
            BughouseClientEvent::MakeTurn{ board_idx, turn_input } => {
                self.process_make_turn(ctx, client_id, now, board_idx, turn_input)
            },
            BughouseClientEvent::CancelPreturn{ board_idx} => {
                self.process_cancel_preturn(ctx, client_id, board_idx)
            },
            BughouseClientEvent::Resign => {
                self.process_resign(ctx, client_id, now)
            },
            BughouseClientEvent::SetReady{ is_ready } => {
                self.process_set_ready(ctx, client_id, is_ready)
            },
            BughouseClientEvent::Leave => {
                self.process_leave(ctx, client_id)
            },
            BughouseClientEvent::UpdateChalkDrawing{ drawing } => {
                self.process_update_chalk_drawing(ctx, client_id, drawing)
            },
            BughouseClientEvent::RequestExport{ format } => {
                self.process_request_export(ctx, client_id, format)
            },
            BughouseClientEvent::ReportPerformace(..) => {
                unreachable!("Contest-independent event must be processed separately");
            },
            BughouseClientEvent::ReportError(..) => {
                unreachable!("Contest-independent event must be processed separately");
            },
            BughouseClientEvent::Ping => {
                unreachable!("Contest-independent event must be processed separately");
            },
        };
        if let Err(err) = result {
            ctx.clients[client_id].send_rejection(err);
        }
    }

    fn join_participant(
        &mut self, ctx: &mut Context, client_id: ClientId, execution: Execution, now: Instant, player_name: String
    ) -> EventResult {
        assert!(ctx.clients[client_id].contest_id.is_none());
        assert!(ctx.clients[client_id].participant_id.is_none());
        if let Some(ref game_state) = self.game_state {
            let existing_participant_id = self.participants.find_by_name(&player_name);
            if let Some(existing_participant_id) = existing_participant_id {
                if let Some(existing_client_id) = ctx.clients.find_participant(existing_participant_id) {
                    // TODO(accounts): Set `is_registered_user` and `is_existing_user_registered`.
                    let is_registered_user = false;
                    let is_existing_user_registered = false;
                    // If both users are registered and have the same user name, then we know for sure
                    // this is the same user. If both are guests, we can only guess. There is no way to
                    // be certain, because guest accounts are inherently non-exclusive.
                    let both_registered = is_registered_user && is_existing_user_registered;
                    let both_guests = !is_registered_user && !is_existing_user_registered;
                    if both_registered {
                        ctx.clients[existing_client_id].send_rejection(BughouseServerRejection::JoinedInAnotherClient);
                        ctx.clients.remove_client(existing_client_id);
                    } else if both_guests && !ctx.clients[existing_client_id].connection_monitor.status(now).is_healthy() {
                        ctx.clients.remove_client(existing_client_id);
                    } else {
                        return Err(BughouseServerRejection::PlayerAlreadyExists{ player_name });
                    }
                } else {
                    if !matches!(execution, Execution::Running) {
                        return Err(BughouseServerRejection::ShuttingDown);
                    }
                }
            }
            let participant_id = existing_participant_id.unwrap_or_else(|| {
                // Improvement potential. Allow joining mid-game in individual mode.
                //   Q. How to balance score in this case?
                self.participants.add_participant(Participant {
                    name: player_name.clone(),
                    faction: Faction::Observer,
                    games_played: 0,
                    is_online: true,
                    is_ready: false,
                })
            });
            ctx.clients[client_id].contest_id = Some(self.contest_id.clone());
            ctx.clients[client_id].participant_id = Some(participant_id);
            ctx.clients[client_id].send(self.make_contest_welcome_event());
            // LobbyUpdated should precede GameStarted, because this is how the client gets their
            // team in FixedTeam mode.
            self.send_lobby_updated(ctx);
            ctx.clients[client_id].send(self.make_game_start_event(now, Some(participant_id)));
            let chalkboard = game_state.chalkboard.clone();
            ctx.clients[client_id].send(BughouseServerEvent::ChalkboardUpdated{ chalkboard });
            Ok(())
        } else {
            let existing_participant_id = self.participants.find_by_name(&player_name);
            if let Some(existing_participant_id) = existing_participant_id {
                if let Some(existing_client_id) = ctx.clients.find_participant(existing_participant_id) {
                    // TODO(accounts): Set `is_registered_user` and `is_existing_user_registered`.
                    let is_registered_user = false;
                    if is_registered_user {
                        let is_existing_user_registered = false;
                        let rejection = if is_existing_user_registered {
                            BughouseServerRejection::JoinedInAnotherClient  // this is us
                        } else {
                            BughouseServerRejection::NameClashWithRegisteredUser
                        };
                        ctx.clients[existing_client_id].send_rejection(rejection);
                        ctx.clients.remove_client(existing_client_id);
                    } else if !ctx.clients[existing_client_id].connection_monitor.status(now).is_healthy() {
                        ctx.clients.remove_client(existing_client_id);
                    } else {
                        return Err(BughouseServerRejection::PlayerAlreadyExists{ player_name });
                    }
                }
            }
            if let Err(reason) = ctx.helpers.validate_player_name(&player_name) {
                return Err(BughouseServerRejection::InvalidPlayerName{ player_name, reason });
            }
            if !matches!(execution, Execution::Running) {
                return Err(BughouseServerRejection::ShuttingDown);
            }
            info!(
                "Client {} join contest {} as {}",
                ctx.clients[client_id].logging_id, self.contest_id.0, player_name
            );
            ctx.clients[client_id].contest_id = Some(self.contest_id.clone());
            let participant_id = self.participants.add_participant(Participant {
                name: player_name,
                faction: Faction::Random,
                games_played: 0,
                is_online: true,
                is_ready: false,
            });
            ctx.clients[client_id].participant_id = Some(participant_id);
            ctx.clients[client_id].send(self.make_contest_welcome_event());
            self.send_lobby_updated(ctx);
            Ok(())
        }
    }

    fn process_set_faction(&mut self, ctx: &mut Context, client_id: ClientId, faction: Faction) -> EventResult {
        let Some(participant_id) = ctx.clients[client_id].participant_id else {
            return Err(unknown_error!("Cannot set faction: not joined"));
        };
        if self.game_state.is_some() {
            return Err(unknown_error!("Cannot set faction: contest already started"));
        }
        match (faction, self.rules.bughouse_rules.teaming) {
            (Faction::Fixed(_), Teaming::FixedTeams) => {}
            (Faction::Fixed(_), Teaming::IndividualMode) => {
                return Err(unknown_error!("Cannot set fixed team in individual mode"));
            },
            (Faction::Random, _) => {},
            (Faction::Observer, _) => {},
        }
        self.participants[participant_id].faction = faction;
        self.send_lobby_updated(ctx);
        Ok(())
    }

    fn process_make_turn(
        &mut self, ctx: &mut Context, client_id: ClientId, now: Instant,
        board_idx: BughouseBoard, turn_input: TurnInput
    ) -> EventResult {
        let Some(GameState{
                ref mut game_start,
                ref mut game_start_offset_time,
                ref mut game_end,
                ref mut game,
                ref mut preturns, ..
            }) = self.game_state else {
            return Err(unknown_error!("Cannot make turn: no game in progress"));
        };
        let Some(participant_id) = ctx.clients[client_id].participant_id else {
            return Err(unknown_error!("Cannot make turn: not joined"));
        };
        let Some(player_bughouse_id) = game.find_player(&self.participants[participant_id].name) else {
            return Err(unknown_error!("Cannot make turn: player does not participate"));
        };
        let Some(envoy) = player_bughouse_id.envoy_for(board_idx) else {
            return Err(unknown_error!("Cannot make turn: player does not play on this board"));
        };
        let scores = &mut self.scores;
        let participants = &mut self.participants;
        let mode = game.turn_mode_for_envoy(envoy);
        match mode {
            Ok(TurnMode::Normal) => {
                let mut turns = vec![];
                let game_now = GameInstant::from_now_game_maybe_active(*game_start, now);
                if let Ok(turn_event) = apply_turn(
                    game_now, envoy, turn_input, game, preturns, participants, scores, game_end, now
                ) {
                    if game_start.is_none() {
                        *game_start = Some(now);
                        *game_start_offset_time = Some(time::OffsetDateTime::now_utc());
                    }
                    turns.push(turn_event);
                    let opponent = envoy.opponent();
                    if let Some(preturn) = preturns.remove(&opponent) {
                        if let Ok(preturn_event) = apply_turn(
                            game_now, opponent, preturn, game, preturns, participants, scores, game_end, now
                        ) {
                            turns.push(preturn_event);
                        } else {
                            // Ignore error: It is completely fine for a preturn to fail.
                        }
                    }
                } else {
                    // Ignore error: We are processing this as a normal turn, but it is possible
                    // that the client hasn't seen last opponent's turn yet and sent this as a
                    // preturn.
                    // This could be made into an error if `BughouseClientEvent::MakeTurn`
                    // distinguishes between in-order turns and preturns (even in this case,
                    // TurnError::GameOver needs to be silenced though).
                }
                let ev = BughouseServerEvent::TurnsMade {
                    turns,
                    game_status: game.status(),
                    scores: scores.clone(),
                };
                self.broadcast(ctx, &ev);
                Ok(())
            },
            Ok(TurnMode::Preturn) => {
                match preturns.entry(envoy) {
                    hash_map::Entry::Occupied(_) => {
                        Err(unknown_error!("Only one premove is supported"))
                    },
                    hash_map::Entry::Vacant(entry) => {
                        entry.insert(turn_input);
                        Ok(())
                    },
                }
            },
            Err(_) => {
                // Ignore error: This could fail if the game ended by flag or on the other board
                // while the turn was in flight.
                Ok(())
            },
        }
    }

    fn process_cancel_preturn(
        &mut self, ctx: &mut Context, client_id: ClientId, board_idx: BughouseBoard
    ) -> EventResult {
        let Some(GameState{ ref game, ref mut preturns, .. }) = self.game_state else {
            return Err(unknown_error!("Cannot cancel pre-turn: no game in progress"));
        };
        let Some(participant_id) = ctx.clients[client_id].participant_id else {
            return Err(unknown_error!("Cannot cancel pre-turn: not joined"));
        };
        let Some(player_bughouse_id) = game.find_player(&self.participants[participant_id].name) else {
            return Err(unknown_error!("Cannot cancel pre-turn: player does not participate"));
        };
        let Some(envoy) = player_bughouse_id.envoy_for(board_idx) else {
            return Err(unknown_error!("Cannot cancel pre-turn: player does not play on this board"));
        };
        preturns.remove(&envoy);
        Ok(())
    }

    fn process_resign(&mut self, ctx: &mut Context, client_id: ClientId, now: Instant) -> EventResult {
        let Some(GameState{ ref mut game, ref mut preturns, game_start, ref mut game_end, .. }) = self.game_state else {
            return Err(unknown_error!("Cannot resign: no game in progress"));
        };
        if game.status() != BughouseGameStatus::Active {
            return Err(unknown_error!("Cannot resign: game already over"));
        }
        let Some(participant_id) = ctx.clients[client_id].participant_id else {
            return Err(unknown_error!("Cannot resign: not joined"));
        };
        let Some(player_bughouse_id) = game.find_player(&self.participants[participant_id].name) else {
            return Err(unknown_error!("Cannot resign: player does not participate"));
        };
        let status = BughouseGameStatus::Victory(
            player_bughouse_id.team().opponent(),
            VictoryReason::Resignation
        );
        let scores = &mut self.scores;
        let participants = &mut self.participants;
        let game_now = GameInstant::from_now_game_maybe_active(game_start, now);
        game.set_status(status, game_now);
        update_state_on_game_over(game, preturns, participants, scores, game_end, now);
        let ev = BughouseServerEvent::GameOver {
            time: game_now,
            game_status: status,
            scores: scores.clone(),
        };
        self.broadcast(ctx, &ev);
        Ok(())
    }

    fn process_set_ready(&mut self, ctx: &mut Context, client_id: ClientId, is_ready: bool) -> EventResult {
        let Some(participant_id) = ctx.clients[client_id].participant_id else {
            return Err(unknown_error!("Cannot update readiness: not joined"));
        };
        if let Some(GameState{ ref game, .. }) = self.game_state {
            if game.status() == BughouseGameStatus::Active {
                return Err(unknown_error!("Cannot update readiness: game still in progress"));
            }
        }
        self.participants[participant_id].is_ready = is_ready;
        self.send_lobby_updated(ctx);
        Ok(())
    }

    fn process_leave(&mut self, ctx: &mut Context, client_id: ClientId) -> EventResult {
        if let Some(logging_id) = ctx.clients.remove_client(client_id) {
            info!("Client {} left", logging_id);
        }
        // Note. Player will be removed automatically. This has to be the case, otherwise
        // clients disconnected due to a network error would've left abandoned players.
        // Improvement potential. Do we really need this event? Clients are removed when the
        // network channel is closed anyway.
        Ok(())
    }

    fn process_update_chalk_drawing(
        &mut self, ctx: &mut Context, client_id: ClientId, drawing: ChalkDrawing
    ) -> EventResult {
        let Some(GameState{ ref mut chalkboard, ref game, .. }) = self.game_state else {
            return Err(unknown_error!("Cannot update chalk drawing: no game in progress"));
        };
        let Some(participant_id) = ctx.clients[client_id].participant_id else {
            return Err(unknown_error!("Cannot update chalk drawing: not joined"));
        };
        if game.status() == BughouseGameStatus::Active {
            return Err(unknown_error!("Cannot update chalk drawing: can draw only after game is over"));
        }
        chalkboard.set_drawing(self.participants[participant_id].name.clone(), drawing);
        let chalkboard = chalkboard.clone();
        self.broadcast(ctx, &BughouseServerEvent::ChalkboardUpdated{ chalkboard });
        Ok(())
    }

    fn process_request_export(
        &self, ctx: &mut Context, client_id: ClientId, format: BughouseExportFormat
    ) -> EventResult {
        let Some(GameState{ ref game, .. }) = self.game_state else {
            return Err(unknown_error!("Cannot export: no game in progress"));
        };
        let all_games = self.match_history.iter().chain(iter::once(game));
        let content = all_games.enumerate().map(|(round, game)| {
            pgn::export_to_bpgn(format, game, round + 1)
        }).join("\n");
        ctx.clients[client_id].send(BughouseServerEvent::GameExportReady{ content });
        Ok(())
    }

    fn post_process(&mut self, ctx: &mut Context, execution: Execution, now: Instant) {
        // Improvement potential: Collapse `send_lobby_updated` events generated during one event
        //   processing cycle. Right now there could up to three: one from the event (SetTeam/SetReady),
        //   one from here and one from `self.start_game`.
        //   Idea: Add `ctx.should_update_lobby` bit and check it in the end.
        // TODO: Show lobby participants as offline when `!c.heart.is_online()`.
        let active_participant_ids: HashSet<_> =
            ctx.clients.map.values().filter_map(|c| c.participant_id).collect();
        let mut lobby_updated = false;
        let mut chalkboard_updated = false;
        self.participants.map.retain(|id, p| {
            let is_online = active_participant_ids.contains(id);
            if !is_online {
                if self.game_state.is_none() {
                    lobby_updated = true;
                    return false;
                }
                if !p.faction.is_player() {
                    if let Some(ref mut game_state) = self.game_state {
                        chalkboard_updated |= game_state.chalkboard.clear_drawings_by_player(p.name.clone());
                    }
                    lobby_updated = true;
                    return false;
                }
            }
            if p.is_online != is_online {
                p.is_online = is_online;
                p.is_ready &= is_online;
                lobby_updated = true;
            }
            true
        });
        if lobby_updated {
            self.send_lobby_updated(ctx);
        }
        if chalkboard_updated {
            let chalkboard = self.game_state.as_ref().unwrap().chalkboard.clone();
            self.broadcast(ctx, &BughouseServerEvent::ChalkboardUpdated{ chalkboard });
        }

        let can_start_game = verify_participants(&self.rules, self.participants.iter()).error.is_none();
        if let Some(first_game_countdown_start) = self.first_game_countdown_since {
            if !can_start_game {
                self.first_game_countdown_since = None;
                self.broadcast(ctx, &BughouseServerEvent::FirstGameCountdownCancelled);
            } else if now.duration_since(first_game_countdown_start) >= FIRST_GAME_COUNTDOWN_DURATION {
                self.start_game(ctx, now);
            }
        } else if can_start_game {
            if !matches!(execution, Execution::Running) {
                self.broadcast(ctx, &BughouseServerEvent::Rejection(BughouseServerRejection::ShuttingDown));
                self.reset_readiness();
                return;
            }
            if let Some(GameState{ ref game, .. }) = self.game_state {
                assert!(game.status() != BughouseGameStatus::Active,
                    "Players must not be allowed to set is_ready flag while the game is active");
                self.match_history.push(game.clone());
                self.start_game(ctx, now);
            } else if self.first_game_countdown_since.is_none() {
                if ctx.disable_countdown {
                    self.start_game(ctx, now);
                } else {
                    // TODO: Add some way of blocking last-moment faction changes, e.g.:
                    //   - Forbid all changes other than resetting readiness;
                    //   - Allow changes, but restart the count-down;
                    //   - Blizzard-style: allow changes during the first half of the count-down.
                    self.broadcast(ctx, &BughouseServerEvent::FirstGameCountdownStarted);
                    self.first_game_countdown_since = Some(now);
                }
            }
        }
    }

    fn start_game(&mut self, ctx: &mut Context, now: Instant) {
        self.reset_readiness();
        self.randomize_fixed_teams();  // non-trivial only in the beginning of a contest
        let players = self.assign_boards();
        let game = BughouseGame::new(
            self.rules.contest_rules.clone(),
            self.rules.chess_rules.clone(),
            self.rules.bughouse_rules.clone(),
            &players
        );
        self.init_scores();
        self.game_state = Some(GameState {
            game,
            game_creation: now,
            game_start: None,
            game_start_offset_time: None,
            game_end: None,
            preturns: HashMap::new(),
            chalkboard: Chalkboard::new(),
        });
        self.broadcast(ctx, &self.make_game_start_event(now, None));
        self.send_lobby_updated(ctx);  // update readiness flags
    }

    fn init_scores(&mut self) {
        match self.rules.bughouse_rules.teaming {
            Teaming::FixedTeams => {},
            Teaming::IndividualMode => {
                assert!(self.scores.per_team.is_empty());
                for p in self.participants.iter() {
                    if p.faction.is_player() {
                        self.scores.per_player.entry(p.name.clone()).or_insert(0);
                    }
                }
            }
        }
    }

    fn make_contest_welcome_event(&self) -> BughouseServerEvent {
        BughouseServerEvent::ContestWelcome {
            contest_id: self.contest_id.0.clone(),
            rules: self.rules.clone(),
        }
    }

    // Creates a game start/reconnect event. `participant_id` is needed only if reconnecting.
    fn make_game_start_event(
        &self, now: Instant, participant_id: Option<ParticipantId>
    ) -> BughouseServerEvent {
        let Some(game_state) = &self.game_state else {
            panic!("Expected ContestState::Game");
        };
        let player_bughouse_id = participant_id
            .and_then(|id| game_state.game.find_player(&self.participants[id].name));
        let preturns = if let Some(player_bughouse_id) = player_bughouse_id {
            player_preturns(&game_state.preturns, player_bughouse_id)
        } else {
            vec![]
        };
        BughouseServerEvent::GameStarted {
            starting_position: game_state.game.starting_position().clone(),
            players: game_state.game.players(),
            time: current_game_time(game_state, now),
            turn_log: game_state.game.turn_log().iter().map(|t| t.trim_for_sending()).collect(),
            preturns,
            game_status: game_state.game.status(),
            scores: self.scores.clone(),
        }
    }

    fn send_lobby_updated(&self, ctx: &mut Context) {
        let participants = self.participants.iter().cloned().collect();
        self.broadcast(ctx, &BughouseServerEvent::LobbyUpdated{ participants });
    }

    fn reset_readiness(&mut self) {
        self.participants.iter_mut().for_each(|p| p.is_ready = false);
    }

    // Randomize fixed teams in the beginning of a contest.
    //
    // Algorithm: shuffle players, then iterate the resulting shuffled array and assign
    // teams. Note that it would be incorrect to go in the player order and assign a random
    // team with a 50/50 probability, or the remaining free team if there's just one.
    // If we were to do this, then the first two players to join would get into the same
    // team with probability 1/2 (instead of 1/3).
    fn randomize_fixed_teams(&mut self) {
        match self.rules.bughouse_rules.teaming {
            Teaming::FixedTeams => {},
            Teaming::IndividualMode => {
                return;
            },
        };
        let mut rng = rand::thread_rng();
        let mut num_fixed = num_fixed_players_per_team(self.participants.iter());
        let mut to_randomize = self.participants.iter_mut()
            .filter(|p| p.faction == Faction::Random)
            .collect_vec();
        to_randomize.shuffle(&mut rng);
        for p in to_randomize {
            let team = Team::iter().min_by_key(|&t| num_fixed[t]).unwrap();
            assert!(num_fixed[team] < TOTAL_ENVOYS_PER_TEAM);
            p.faction = Faction::Fixed(team);
            num_fixed[team] += 1;
        }
    }

    // Improvement potential: In a game with three players also balance the number of times
    //   each person double-plays.
    // TODO: Add unit tests for `assign_boards` since it's not covered by `bughouse_online.rs`.
    fn assign_boards(&self) -> Vec<PlayerInGame> {
        if let Some(assignment) = &self.board_assignment_override {
            for player_assignment in assignment {
                if let Some(player) = self.participants.iter().find(|p| p.name == player_assignment.name) {
                    if let Faction::Fixed(team) = player.faction {
                        assert_eq!(team, player_assignment.id.team());
                    }
                }
            }
            return assignment.clone();
        }

        let mut rng = rand::thread_rng();
        let mut players_per_team = enum_map!{ _ => vec![] };
        match self.rules.bughouse_rules.teaming {
            Teaming::FixedTeams => {
                for p in self.participants.iter() {
                    match p.faction {
                        Faction::Fixed(team) => players_per_team[team].push(p.clone()),
                        Faction::Random => panic!("Player {} doesn't have a team in team mode", p.name),
                        Faction::Observer => {},
                    }
                }
            },
            Teaming::IndividualMode => {
                let participants = self.participants.iter()
                    .filter(|p| match p.faction {
                        Faction::Fixed(_) => panic!("Player {} has a fixed team in individual mode", p.name),
                        Faction::Random => true,
                        Faction::Observer => false,
                    });
                for p in get_next_players(participants, TOTAL_ENVOYS) {
                    let team = Team::iter().min_by_key(|&t| players_per_team[t].len()).unwrap();
                    assert!(players_per_team[team].len() < TOTAL_ENVOYS_PER_TEAM);
                    players_per_team[team].push(p);
                }
            },
        }
        players_per_team.into_iter().map(|(team, mut team_players)| {
            team_players.shuffle(&mut rng);
            match team_players.len() {
                0 => panic!("Empty team: {}", team_players.len()),
                1 => {
                    vec![
                        PlayerInGame {
                            name: team_players.pop().unwrap().name.clone(),
                            id: BughousePlayer::DoublePlayer(team)
                        }
                    ]
                },
                _ => {
                    BughouseBoard::iter()
                        .zip(get_next_players(team_players.iter(), TOTAL_ENVOYS_PER_TEAM).into_iter())
                        .map(move |(board_idx, participant)| PlayerInGame {
                            name: participant.name.clone(),
                            id: BughousePlayer::SinglePlayer(BughouseEnvoy {
                                board_idx,
                                force: get_bughouse_force(team, board_idx)
                            })
                        })
                        .collect_vec()
                },
            }
        }).flatten().collect_vec()
    }
}

fn current_game_time(game_state: &GameState, now: Instant) -> GameInstant {
    if game_state.game.status() == BughouseGameStatus::Active {
        GameInstant::from_now_game_maybe_active(game_state.game_start, now)
    } else {
        // Normally `clock().total_time_elapsed()` should be the same on all boards. But
        // it could differ in case the of a flag defeat. Consider the following situation:
        // total time is 300 seconds; on board A the game proceeded normally; on board B
        // white didn't make any turns. In this case board A clock would report real wall
        // time (e.g. 300.1s), while board B clock would report exactly 300s, because each
        // player remaining time is always non-negative.
        // Also this example shows that the best approximation to real game time is the
        // minimum of all boards. Everything higher than the minimum is an artifact of not
        // having checked the flags in time.
        let elapsed_since_start = BughouseBoard::iter()
            .map(|board_idx| game_state.game.board(board_idx).clock().total_time_elapsed())
            .min()
            .unwrap();
        GameInstant::from_duration(elapsed_since_start)
    }
}

fn apply_turn(
    game_now: GameInstant, envoy: BughouseEnvoy, turn_input: TurnInput, game: &mut BughouseGame,
    preturns: &mut Preturns, participants: &mut Participants, scores: &mut Scores,
    game_end: &mut Option<Instant>, now: Instant,
) -> Result<TurnRecord, TurnError> {
    game.try_turn_by_envoy(envoy, &turn_input, TurnMode::Normal, game_now)?;
    if game.status() != BughouseGameStatus::Active {
        update_state_on_game_over(game, preturns, participants, scores, game_end, now);
    }
    Ok(game.last_turn_record().unwrap().trim_for_sending())
}

fn update_state_on_game_over(
    game: &BughouseGame, preturns: &mut Preturns, participants: &mut Participants, scores: &mut Scores,
    game_end: &mut Option<Instant>, now: Instant,
) {
    assert!(game_end.is_none());
    *game_end = Some(now);
    preturns.clear();
    let team_scores = match game.status() {
        BughouseGameStatus::Active => panic!("It just so happens that the game here is only mostly over"),
        BughouseGameStatus::Victory(team, _) => {
            let mut s = enum_map!{ _ => 0 };
            s[team] = 2;
            s
        },
        BughouseGameStatus::Draw(_) => enum_map!{ _ => 1 },
    };
    match game.bughouse_rules().teaming {
        Teaming::FixedTeams => {
            assert!(scores.per_player.is_empty());
            for (team, score) in team_scores {
                *scores.per_team.entry(team).or_insert(0) += score;
            }
        },
        Teaming::IndividualMode => {
            assert!(scores.per_team.is_empty());
            for p in game.players() {
                *scores.per_player.entry(p.name.clone()).or_insert(0) += team_scores[p.id.team()];
            }
        }
    }
    let player_names: HashSet<_> = game.players().into_iter().map(|p| p.name).collect();
    for p in participants.iter_mut() {
        if player_names.contains(&p.name) {
            p.games_played += 1;
        }
    }
}

fn player_preturns(preturns: &Preturns, player: BughousePlayer) -> Vec<(BughouseBoard, TurnInput)> {
    let preturns_or = match player {
        BughousePlayer::SinglePlayer(envoy) => vec![
            (envoy.board_idx, preturns.get(&envoy))
        ],
        BughousePlayer::DoublePlayer(team) => BughouseBoard::iter().map(|board_idx| {
            let envoy = BughouseEnvoy{ board_idx, force: get_bughouse_force(team, board_idx) };
            (board_idx, preturns.get(&envoy))
        }).collect_vec(),
    };
    return preturns_or.into_iter()
        .filter(|(_, turn)| turn.is_some())
        .map(|(board_idx, turn)| (board_idx, turn.unwrap().clone()))
        .collect();
}

fn get_next_players<'a>(
    participants: impl Iterator<Item = &'a Participant>, max_players: usize
) -> Vec<Participant> {
    let mut rng = rand::thread_rng();
    let players_buckets = participants
        .sorted_by_key(|p| p.games_played)
        .group_by(|p| p.games_played);
    let mut ret = Vec::<Participant>::new();
    for (_, bucket) in players_buckets.into_iter() {
        let bucket = bucket.collect_vec();
        let seats_left = max_players - ret.len();
        let n = cmp::min(bucket.len(), seats_left);
        ret.extend(bucket.choose_multiple(&mut rng, n).cloned().cloned());
    }
    ret.shuffle(&mut rng);
    ret
}

fn process_report_error(ctx: &Context, client_id: ClientId, report: &BughouseClientErrorReport) {
    // TODO: Save errors to DB.
    let logging_id = &ctx.clients[client_id].logging_id;
    match report {
        BughouseClientErrorReport::RustPanic{ panic_info, backtrace } => {
            warn!("Client {logging_id} panicked:\n{panic_info}\nBacktrace: {backtrace}");
        }
        BughouseClientErrorReport::RustError{ message } => {
            warn!("Client {logging_id} experienced Rust error:\n{message}");
        }
        BughouseClientErrorReport::UnknownError{ message } => {
            warn!("Client {logging_id} experienced unknown error:\n{message}");
        }
    }
}

fn process_ping(ctx: &mut Context, client_id: ClientId) {
    ctx.clients[client_id].send(BughouseServerEvent::Pong);
}

fn shutdown() {
    // Note. It may seem like a good idea to terminate the process "properly": join threads, call
    // destructors, etc. But I think it's actually not. By aborting the process during the normal
    // shutdown procedure we make sure that this path is tested and thus abnormal shutdown (panic
    // or VM failure) does not lose data.
    std::process::exit(0);
}
