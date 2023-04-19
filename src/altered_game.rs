// Defines `AlteredGame` and auxiliary classes. `AlteredGame` is used for online multiplayer
// and represents a game with local changes not (yet) confirmed by the server.
//
// The general philosophy is that server is trusted, but the user is not. `AlteredGame` may
// (or may not) panic if server command doesn't make sense (e.g. invalid chess move), but it
// shall not panic on bogus local turns and other invalid user actions.
//
// Only one preturn is allowed (for game-design reasons, this is not a technical limitation).
// It is still possible to have two unconfirmed local turns: one normal and one preturn.

// Improvement potential: Reduce the number of times large entities are recomputed
// (e.g.`turn_highlights` recomputes `local_game` and `fog_of_war_area`, which are presumably
// already available by the time it's used). Ideas:
//   - Cache latest result and reevaluate when invalidated;
//   - Replace all existing read-only methods with one "get visual representation" method that
//     contain all the information required in order to render the game.

use std::collections::HashSet;
use std::rc::Rc;

use enum_map::{enum_map, EnumMap};
use itertools::Itertools;
use strum::IntoEnumIterator;

use crate::board::{Turn, TurnDrop, TurnError, TurnExpanded, TurnInput, TurnMode, TurnMove};
use crate::clock::GameInstant;
use crate::coord::{Coord, SubjectiveRow};
use crate::display::Perspective;
use crate::game::{
    get_bughouse_force, BughouseBoard, BughouseEnvoy, BughouseGame, BughouseGameStatus,
    BughouseParticipant, TurnRecord, TurnRecordExpanded,
};
use crate::piece::{CastleDirection, PieceForce, PieceKind};
use crate::rules::{BughouseRules, ChessRules, ChessVariant};


#[derive(Clone, Copy, Debug)]
pub enum PieceDragStart {
    Board(Coord),
    Reserve(PieceKind),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PieceDragError {
    DragForbidden,
    DragAlreadyStarted,
    NoDragInProgress,
    DragNoLongerPossible,
    PieceNotFound,
    Cancelled,
}

#[derive(Clone, Copy, Debug)]
pub enum PieceDragSource {
    Defunct, // dragged piece captured by opponent or depends on a cancelled preturn
    Board(Coord),
    Reserve,
}

#[derive(Clone, Debug)]
pub struct PieceDrag {
    pub board_idx: BughouseBoard,
    pub piece_kind: PieceKind,
    pub piece_force: PieceForce,
    pub source: PieceDragSource,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TurnHighlightLayer {
    BelowFog,
    AboveFog,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TurnHighlightFamily {
    Preturn,
    LatestTurn,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TurnHighlightItem {
    MoveFrom,
    MoveTo,
    Drop,
    Capture,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TurnHighlight {
    pub board_idx: BughouseBoard,
    pub coord: Coord,
    pub layer: TurnHighlightLayer,
    pub family: TurnHighlightFamily,
    pub item: TurnHighlightItem,
}

#[derive(Clone, Debug)]
pub struct AlteredGame {
    // All local actions are assumed to be made on behalf of this player.
    my_id: BughouseParticipant,
    // State as it has been confirmed by the server.
    game_confirmed: BughouseGame,
    // Local changes of two kinds:
    //   - Local turns (TurnMode::Normal) not confirmed by the server yet, but displayed on the
    //     client. Always valid turns for the `game_confirmed`.
    //   - Local preturns (TurnMode::Preturn).
    // The turns are executed sequentially. Preturns always follow normal turns.
    local_turns: Vec<TurnRecord>,
    // Historical position that the user is currently viewing.
    wayback_turn_index: EnumMap<BughouseBoard, Option<String>>,
    // Drag&drop state if making turn by mouse or touch.
    piece_drag: Option<PieceDrag>,
}

impl AlteredGame {
    pub fn new(my_id: BughouseParticipant, game_confirmed: BughouseGame) -> Self {
        AlteredGame {
            my_id,
            game_confirmed,
            local_turns: Vec::new(),
            wayback_turn_index: enum_map! { _ => None },
            piece_drag: None,
        }
    }

    pub fn chess_rules(&self) -> &Rc<ChessRules> { self.game_confirmed().chess_rules() }
    pub fn bughouse_rules(&self) -> &Rc<BughouseRules> { self.game_confirmed().bughouse_rules() }

    // Status returned by this function may differ from `local_game()` status.
    // This function should be used as the source of truth when showing game status to the
    // user, as it's possible that the final status from the server will be different, e.g.
    // if the game ended earlier on the other board.
    pub fn status(&self) -> BughouseGameStatus { self.game_confirmed.status() }
    pub fn is_active(&self) -> bool { self.game_confirmed.is_active() }

    pub fn set_status(&mut self, status: BughouseGameStatus, time: GameInstant) {
        assert!(!status.is_active());
        self.reset_local_changes();
        self.game_confirmed.set_status(status, time);
    }

    pub fn apply_remote_turn(
        &mut self, envoy: BughouseEnvoy, turn_input: &TurnInput, time: GameInstant,
    ) -> Result<Turn, TurnError> {
        let mut original_game_confirmed = self.game_confirmed.clone();
        let turn =
            self.game_confirmed
                .try_turn_by_envoy(envoy, turn_input, TurnMode::Normal, time)?;

        if !self.game_confirmed.is_active() {
            self.reset_local_changes();
            return Ok(turn);
        }

        for (turn_idx, turn_record) in self.local_turns.iter().enumerate() {
            if turn_record.envoy == envoy {
                let local_turn =
                    original_game_confirmed.apply_turn_record(turn_record, TurnMode::Normal);
                if local_turn == Ok(turn) {
                    // The server confirmed a turn made by this player. Discard the local copy.
                    self.local_turns.remove(turn_idx);
                    break;
                } else {
                    // The server sent a turn made by this player, but it's different from the local
                    // turn. The entire turn sequence on this board is now invalid. One way this
                    // could have happened is if the user made a preturn, cancelled it and made new
                    // preturns locally, but the cancellation didn't get to the server in time, so
                    // the server applied earlier preturn version. We don't want to apply subsequent
                    // preturns based on a different game history: if the first turn changed, the
                    // user probably wants to make different turns based on that.
                    self.local_turns.retain(|r| r.envoy != envoy);
                    break;
                }
            }
        }
        self.discard_invalid_local_turns();

        let mut game = self.game_with_local_turns(true);
        if self.apply_drag(&mut game).is_err() {
            let Some(ref mut drag) = self.piece_drag else {
                panic!("Got a drag failure with no drag in progress");
            };
            // Drag invalidated. Possible reasons: dragged piece was captured by opponent;
            // dragged piece depended on a preturn that was cancelled.
            drag.source = PieceDragSource::Defunct;
        }
        Ok(turn)
    }

    pub fn my_id(&self) -> BughouseParticipant { self.my_id }
    pub fn perspective(&self) -> Perspective { Perspective::for_participant(self.my_id) }
    pub fn game_confirmed(&self) -> &BughouseGame { &self.game_confirmed }

    pub fn local_game(&self) -> BughouseGame {
        let mut game = self.game_with_local_turns(true);
        self.apply_wayback(&mut game);
        self.apply_drag(&mut game).unwrap();
        game
    }

    pub fn turn_highlights(&self) -> Vec<TurnHighlight> {
        let my_id = self.my_id;
        let game = self.local_game();
        let mut highlights = vec![];

        for board_idx in BughouseBoard::iter() {
            let see_though_fog = self.see_though_fog();
            let empty_area = HashSet::new();
            let fog_render_area = self.fog_of_war_area(board_idx);
            let fog_cover_area = if see_though_fog { &empty_area } else { &fog_render_area };

            let wayback_turn_idx = self.wayback_turn_index(board_idx);
            let turn_log = board_turn_log_modulo_wayback(&game, board_idx, wayback_turn_idx);

            if let Some(latest_turn_record) = turn_log.last() {
                if !my_id.plays_for(latest_turn_record.envoy) || wayback_turn_idx.is_some() {
                    // Highlight all components of the latest megaturn. Normally this would be exactly one
                    // turn, but in duck chess it's both the regular piece and the duck.
                    for r in
                        turn_log.iter().rev().take_while(|r| r.envoy == latest_turn_record.envoy)
                    {
                        highlights.extend(get_turn_highlights(
                            TurnHighlightFamily::LatestTurn,
                            board_idx,
                            &r.turn_expanded,
                            fog_cover_area,
                        ));
                    }
                }
            }

            for r in turn_log.iter() {
                if r.mode == TurnMode::Preturn {
                    highlights.extend(get_turn_highlights(
                        TurnHighlightFamily::Preturn,
                        board_idx,
                        &r.turn_expanded,
                        fog_cover_area,
                    ));
                }
            }
        }
        highlights
            .into_iter()
            .into_grouping_map_by(|h| (h.board_idx, h.coord))
            .max_by_key(|_, h| turn_highlight_z_index(h))
            .into_values()
            .collect()
    }

    pub fn see_though_fog(&self) -> bool { !self.is_active() }

    pub fn fog_of_war_area(&self, board_idx: BughouseBoard) -> HashSet<Coord> {
        match self.chess_rules().chess_variant {
            ChessVariant::Standard => HashSet::new(),
            ChessVariant::FogOfWar => {
                if let BughouseParticipant::Player(my_player_id) = self.my_id {
                    // Don't use `local_game`: preturns and drags should not reveal new areas.
                    let mut game = self.game_with_local_turns(false);
                    let wayback_active = self.apply_wayback_for_board(&mut game, board_idx);
                    let force = get_bughouse_force(my_player_id.team(), board_idx);
                    let mut visible = game.board(board_idx).fog_free_area(force);
                    // ... but do show preturn pieces themselves:
                    if !wayback_active {
                        let game_with_preturns = self.game_with_local_turns(true);
                        for coord in Coord::all() {
                            if let Some(piece) = game_with_preturns.board(board_idx).grid()[coord] {
                                if piece.force.is_owned_by_or_neutral(force) {
                                    visible.insert(coord);
                                }
                            }
                        }
                    }
                    Coord::all().filter(|c| !visible.contains(c)).collect()
                } else {
                    HashSet::new()
                }
            }
        }
    }

    pub fn try_local_turn(
        &mut self, board_idx: BughouseBoard, turn_input: TurnInput, time: GameInstant,
    ) -> Result<TurnMode, TurnError> {
        let mode = self.try_local_turn_ignore_drag(board_idx, turn_input, time)?;
        self.piece_drag = None;
        Ok(mode)
    }

    pub fn wayback_turn_index(&self, board_idx: BughouseBoard) -> Option<&str> {
        self.wayback_turn_index[board_idx].as_deref()
    }
    pub fn wayback_to_turn(&mut self, board_idx: BughouseBoard, turn_idx: Option<String>) {
        self.abort_drag_piece_on_board(board_idx);
        let last_turn_idx = self
            .local_game()
            .turn_log()
            .iter()
            .rev()
            .find(|record| record.envoy.board_idx == board_idx)
            .map(|record| record.index());
        let turn_idx = if turn_idx == last_turn_idx { None } else { turn_idx };
        self.wayback_turn_index[board_idx] = turn_idx;
    }

    pub fn piece_drag_state(&self) -> &Option<PieceDrag> { &self.piece_drag }

    pub fn start_drag_piece(
        &mut self, board_idx: BughouseBoard, start: PieceDragStart,
    ) -> Result<(), PieceDragError> {
        let BughouseParticipant::Player(my_player_id) = self.my_id else {
            return Err(PieceDragError::DragForbidden);
        };
        let Some(my_envoy) = my_player_id.envoy_for(board_idx) else {
            return Err(PieceDragError::DragForbidden);
        };
        if self.wayback_turn_index[board_idx].is_some() {
            return Err(PieceDragError::DragForbidden);
        }
        if self.piece_drag.is_some() {
            return Err(PieceDragError::DragAlreadyStarted);
        }
        let game = self.game_with_local_turns(true);
        let board = game.board(board_idx);
        let (piece_kind, piece_force, source) = match start {
            PieceDragStart::Board(coord) => {
                let piece = board.grid()[coord].ok_or(PieceDragError::PieceNotFound)?;
                if !board.can_potentially_move_piece(my_envoy.force, piece.force) {
                    return Err(PieceDragError::DragForbidden);
                }
                (piece.kind, piece.force, PieceDragSource::Board(coord))
            }
            PieceDragStart::Reserve(piece_kind) => {
                if board.reserve(my_envoy.force)[piece_kind] <= 0 {
                    return Err(PieceDragError::PieceNotFound);
                }
                let piece_force = piece_kind.reserve_piece_force(my_envoy.force);
                (piece_kind, piece_force, PieceDragSource::Reserve)
            }
        };
        self.piece_drag = Some(PieceDrag {
            board_idx,
            piece_kind,
            piece_force,
            source,
        });
        Ok(())
    }

    pub fn abort_drag_piece(&mut self) { self.piece_drag = None; }

    pub fn abort_drag_piece_on_board(&mut self, board_idx: BughouseBoard) {
        if let Some(ref mut drag) = self.piece_drag {
            if drag.board_idx == board_idx {
                self.piece_drag = None;
            }
        }
    }

    // Stop drag and returns turn on success. The client should then manually apply this
    // turn via `make_turn`.
    pub fn drag_piece_drop(
        &mut self, dest: Coord, promote_to: PieceKind,
    ) -> Result<TurnInput, PieceDragError> {
        let PieceDrag { piece_kind, piece_force, source, .. } =
            self.piece_drag.take().ok_or(PieceDragError::NoDragInProgress)?;

        match source {
            PieceDragSource::Defunct => Err(PieceDragError::DragNoLongerPossible),
            PieceDragSource::Board(source_coord) => {
                use PieceKind::*;
                if source_coord == dest {
                    return Err(PieceDragError::Cancelled);
                }
                if piece_kind == PieceKind::Duck {
                    return Ok(TurnInput::DragDrop(Turn::PlaceDuck(dest)));
                }
                let d_col = dest.col - source_coord.col;
                let mut is_castling = false;
                let mut is_promotion = false;
                if let Ok(force) = piece_force.try_into() {
                    let first_row = SubjectiveRow::from_one_based(1).unwrap().to_row(force);
                    let last_row = SubjectiveRow::from_one_based(8).unwrap().to_row(force);
                    is_castling = piece_kind == King
                        && (d_col.abs() >= 2)
                        && (source_coord.row == first_row && dest.row == first_row);
                    is_promotion = piece_kind == Pawn && dest.row == last_row;
                }
                if is_castling {
                    use CastleDirection::*;
                    let dir = if d_col > 0 { HSide } else { ASide };
                    Ok(TurnInput::DragDrop(Turn::Castle(dir)))
                } else {
                    Ok(TurnInput::DragDrop(Turn::Move(TurnMove {
                        from: source_coord,
                        to: dest,
                        promote_to: if is_promotion { Some(promote_to) } else { None },
                    })))
                }
            }
            PieceDragSource::Reserve => {
                if piece_kind == PieceKind::Duck {
                    return Ok(TurnInput::DragDrop(Turn::PlaceDuck(dest)));
                }
                Ok(TurnInput::DragDrop(Turn::Drop(TurnDrop { piece_kind, to: dest })))
            }
        }
    }

    pub fn cancel_preturn(&mut self, board_idx: BughouseBoard) -> bool {
        // Note: Abort drag just to be safe. In practice existing GUI doesn't allow to
        // cancel preturn while dragging. If this is desired, a proper check needs to be
        // done (like in `apply_remote_turn_algebraic`).
        self.piece_drag = None;
        if self.num_preturns_on_board(board_idx) == 0 {
            return false;
        }
        for (turn_idx, turn_record) in self.local_turns.iter().enumerate().rev() {
            if turn_record.envoy.board_idx == board_idx {
                self.local_turns.remove(turn_idx);
                return true;
            }
        }
        unreachable!(); // must have found a preturn, since num_preturns_on_board > 0
    }

    fn reset_local_changes(&mut self) {
        self.local_turns.clear();
        self.piece_drag = None;
    }

    fn discard_invalid_local_turns(&mut self) {
        // Although we don't allow it currently, this function is written in a way that supports
        // turns cross-board turn dependencies.
        let mut game = self.game_confirmed.clone();
        let mut is_board_ok = enum_map! { _ => true };
        self.local_turns.retain(|turn_record| {
            let is_ok = game
                .turn_mode_for_envoy(turn_record.envoy)
                .and_then(|mode| game.apply_turn_record(&turn_record, mode))
                .is_ok();
            if !is_ok {
                // Whenever we find an invalid turn, discard all subsequent turns on that board.
                is_board_ok[turn_record.envoy.board_idx] = false;
            }
            is_board_ok[turn_record.envoy.board_idx]
        });
    }

    fn game_with_local_turns(&self, include_preturns: bool) -> BughouseGame {
        let mut game = self.game_confirmed.clone();
        for turn_record in self.local_turns.iter() {
            // Note. Not calling `test_flag`, because only server records flag defeat.
            // Unwrap ok: turn correctness (according to the `mode`) has already been verified.
            let mode = game.turn_mode_for_envoy(turn_record.envoy).unwrap();
            if mode == TurnMode::Preturn && !include_preturns {
                // Do not break because we can still get in-order turns on the other board.
                continue;
            }
            game.apply_turn_record(&turn_record, mode).unwrap();
        }
        game
    }

    fn num_preturns_on_board(&self, board_idx: BughouseBoard) -> usize {
        let mut game = self.game_confirmed.clone();
        let mut num_preturns = 0;
        for turn_record in self.local_turns.iter() {
            // Unwrap ok: turn correctness (according to the `mode`) has already been verified.
            let mode = game.turn_mode_for_envoy(turn_record.envoy).unwrap();
            if turn_record.envoy.board_idx == board_idx && mode == TurnMode::Preturn {
                num_preturns += 1;
            }
            game.apply_turn_record(&turn_record, mode).unwrap();
        }
        num_preturns
    }

    fn apply_wayback_for_board(&self, game: &mut BughouseGame, board_idx: BughouseBoard) -> bool {
        let Some(ref turn_idx) = self.wayback_turn_index[board_idx] else {
            return false;
        };
        for turn in game.turn_log() {
            if turn.envoy.board_idx == board_idx && turn.index() >= *turn_idx {
                let turn_time = turn.time;
                let board_after = turn.board_after.clone();
                let board = game.board_mut(board_idx);
                *board = board_after;
                board.clock_mut().stop(turn_time);
                break;
            }
        }
        true
    }

    fn apply_wayback(&self, game: &mut BughouseGame) {
        for board_idx in BughouseBoard::iter() {
            self.apply_wayback_for_board(game, board_idx);
        }
    }

    fn apply_drag(&self, game: &mut BughouseGame) -> Result<(), ()> {
        let Some(ref drag) = self.piece_drag else {
            return Ok(());
        };
        let envoy = self.my_id.envoy_for(drag.board_idx).ok_or(())?;
        let board = game.board_mut(drag.board_idx);
        match drag.source {
            PieceDragSource::Defunct => {}
            PieceDragSource::Board(coord) => {
                // Note: `take` modifies the board
                let piece = board.grid_mut()[coord].take().ok_or(())?;
                let expected = (drag.piece_force, drag.piece_kind);
                let actual = (piece.force, piece.kind);
                if expected != actual {
                    return Err(());
                }
            }
            PieceDragSource::Reserve => {
                let reserve = board.reserve_mut(envoy.force);
                if reserve[drag.piece_kind] <= 0 {
                    return Err(());
                }
                reserve[drag.piece_kind] -= 1;
            }
        }
        Ok(())
    }

    fn try_local_turn_ignore_drag(
        &mut self, board_idx: BughouseBoard, turn_input: TurnInput, time: GameInstant,
    ) -> Result<TurnMode, TurnError> {
        let Some(envoy) = self.my_id.envoy_for(board_idx) else {
            return Err(TurnError::NotPlayer);
        };
        if self.wayback_turn_index[board_idx].is_some() {
            return Err(TurnError::WaybackIsActive);
        }
        if self.num_preturns_on_board(board_idx) >= self.chess_rules().max_preturns_per_board() {
            return Err(TurnError::PreturnLimitReached);
        }
        let mut game = self.game_with_local_turns(true);
        let mode = game.turn_mode_for_envoy(envoy)?;
        game.try_turn_by_envoy(envoy, &turn_input, mode, time)?;
        // Note: cannot use `game.turn_log().last()` here! It will change the input method, and this
        // can cause subtle differences in preturn execution. For example, when making algebraic
        // turns you can require the turn be capturing by using "x". This information will be lost
        // if using TurnInput::Explicit.
        self.local_turns.push(TurnRecord { envoy, turn_input, time });
        Ok(mode)
    }
}

fn board_turn_log_modulo_wayback(
    game: &BughouseGame, board_idx: BughouseBoard, wayback_turn_idx: Option<&str>,
) -> Vec<TurnRecordExpanded> {
    let mut turn_log = game
        .turn_log()
        .iter()
        .filter(|r| r.envoy.board_idx == board_idx)
        .cloned()
        .collect_vec();
    if let Some(wayback_turn_idx) = wayback_turn_idx {
        let mut wayback_turn_found = false;
        turn_log.retain(|r| {
            // The first turn with this condition should be kept, the rest should be deleted.
            if r.index().as_str() >= wayback_turn_idx {
                if !wayback_turn_found {
                    wayback_turn_found = true;
                    true
                } else {
                    false
                }
            } else {
                true
            }
        });
    }
    turn_log
}

// Tuple values are compared lexicographically. Higher values overshadow lower values.
fn turn_highlight_z_index(highlight: &TurnHighlight) -> (u8, u8, u8) {
    (
        match highlight.layer {
            TurnHighlightLayer::AboveFog => 1,
            TurnHighlightLayer::BelowFog => 0,
        },
        match highlight.family {
            TurnHighlightFamily::Preturn => 1,
            TurnHighlightFamily::LatestTurn => 0,
        },
        match highlight.item {
            // Capture coincides with MoveTo (except for en-passant) and should take priority.
            // Whether MoveTo is above MoveFrom determines how sequences of preturns with a single
            // piece are rendered. The current approach if to highlight intermediate squares as
            // MoveTo, but this is a pretty arbitrary choice.
            TurnHighlightItem::Capture => 3,
            TurnHighlightItem::MoveTo => 2,
            TurnHighlightItem::Drop => 1,
            TurnHighlightItem::MoveFrom => 0,
        },
    )
}

fn get_turn_highlight_basis(turn_expanded: &TurnExpanded) -> Vec<(TurnHighlightItem, Coord)> {
    let mut highlights = vec![];
    if let Some((from, to)) = turn_expanded.relocation {
        highlights.push((TurnHighlightItem::MoveFrom, from));
        highlights.push((TurnHighlightItem::MoveTo, to));
    }
    if let Some((from, to)) = turn_expanded.relocation_extra {
        highlights.push((TurnHighlightItem::MoveFrom, from));
        highlights.push((TurnHighlightItem::MoveTo, to));
    }
    if let Some(drop) = turn_expanded.drop {
        highlights.push((TurnHighlightItem::Drop, drop));
    }
    if let Some(ref capture) = turn_expanded.capture {
        highlights.push((TurnHighlightItem::Capture, capture.from));
    }
    highlights
}

fn get_turn_highlights(
    family: TurnHighlightFamily, board_idx: BughouseBoard, turn: &TurnExpanded,
    fog_of_war_area: &HashSet<Coord>,
) -> Vec<TurnHighlight> {
    get_turn_highlight_basis(turn)
        .into_iter()
        .filter_map(|(item, coord)| {
            // Highlights of all visible squares should be rendered below the fog. Semantically
            // there is no difference: the highlight will be visible anyway. But visually it's more
            // appealing in cases when a highlight overlaps with a fog sprite extending from a
            // neighboring square.
            let mut layer = TurnHighlightLayer::BelowFog;

            // Check whether the highlight is visible in the fog.
            if fog_of_war_area.contains(&coord) {
                if item == TurnHighlightItem::Capture {
                    // A piece owned by the current player before it was captured. Location
                    // information is known to the player.
                    layer = TurnHighlightLayer::AboveFog;
                } else {
                    return None;
                }
            }

            Some(TurnHighlight { family, item, board_idx, coord, layer })
        })
        .collect()
}
